use {
    std::cmp::Reverse,
    crate::config::{Aggregation, Step, Zone},
};

/// Map a temperature to a duty cycle through the zone's steps
pub fn duty_for_temp(steps: &[Step], temp: u8) -> u8 {
    let result = steps.binary_search_by(|s| s.temp.cmp(&temp));
    // Index of first step >= the current temperature (if exists)
    let above_index = match result {
        Ok(i) => Some(i),
        Err(i) if i == steps.len() => None,
        Err(i) => Some(i),
    };
    // Index of first step < the current temperature (if exists)
    let below_index = match above_index {
        Some(0) => None,
        Some(i) => Some(i - 1),
        None => None,
    };
    // If step above doesn't exist, use last step's dcycle or 100%
    let above_step = match above_index {
        Some(i) => steps[i],
        None => {
            let dcycle = steps.last()
                .map_or(100, |s| s.dcycle);

            Step {
                temp,
                dcycle,
            }
        }
    };
    // If step below doesn't exist, use same step as step above
    let below_step = match below_index {
        Some(i) => steps[i],
        None => above_step,
    };

    if below_step.temp == above_step.temp {
        below_step.dcycle
    } else {
        // Linearly scale the dcycle
        (u32::from(temp - below_step.temp)
            * u32::from(above_step.dcycle - below_step.dcycle)
            / u32::from(above_step.temp - below_step.temp)
            + u32::from(below_step.dcycle)) as u8
    }
}

/// How an average that falls between two integers is made one
#[derive(Clone, Copy)]
pub enum Rounding {
    /// Toward zero, for temperatures
    Down,
    /// Away from zero, for duties, so the fans never run slower than the mean
    Up,
}

/// Combine readings, either temperatures or duties, into one value. The
/// average divides by the number of readings it used.
pub fn aggregate(readings: &[u8], aggregation: &Aggregation, rounding: Rounding) -> u8 {
    let mut readings = readings.to_vec();
    readings.sort_by_key(|r| Reverse(*r));

    // Callers never pass an empty slice: ZoneState only aggregates groups
    // that hold at least one value
    match aggregation {
        Aggregation::Maximum => {
            readings.first().copied().unwrap()
        }
        Aggregation::Average { top } => {
            let n = top.unwrap_or(readings.len()).min(readings.len());

            let sum = readings
                .into_iter()
                .take(n)
                .map(u64::from)
                .sum::<u64>();
            let count = n as u64;

            let dividend = match rounding {
                Rounding::Down => sum,
                Rounding::Up => sum + count.saturating_sub(1),
            };

            // A mean of u8 values fits in a u8; an empty slice gives 0
            dividend.checked_div(count).unwrap_or(0) as u8
        }
    }
}

/// Duty for a zone when its readings cannot be mapped through its curves
const FULL_DUTY: u8 = 100;

/// The temperature a curve maps, held through falls smaller than the
/// hysteresis
struct HeldTemp(Option<u8>);

impl HeldTemp {
    /// Take a new temperature and return the one the curve should map
    fn update(&mut self, temp: u8, hysteresis: u8) -> u8 {
        let held = match self.0 {
            Some(held) if temp < held && held - temp < hysteresis => held,
            _ => temp,
        };

        self.0 = Some(held);
        held
    }
}

pub struct ZoneUpdate {
    /// The duty cycle to set for the zone
    pub duty: u8,
    /// The temperature each curve mapped: the zone curve first when it is
    /// used, then the curve of each source with `steps`, in source order
    pub curve_temps: Vec<u8>,
}

/// The held temperatures of a zone's curves between polls
pub struct ZoneState {
    zone_curve: HeldTemp,
    /// One per source; only sources with `steps` use theirs
    source_curves: Vec<HeldTemp>,
}

impl ZoneState {
    pub fn new(zone: &Zone) -> Self {
        Self {
            zone_curve: HeldTemp(None),
            source_curves: zone.sources.iter().map(|_| HeldTemp(None)).collect(),
        }
    }

    /// Compute the zone duty from one reading per source, in source order.
    /// Sources without `steps` are aggregated into the zone curve; each source
    /// with `steps` maps its reading through its own curve.
    pub fn next_duty(&mut self, zone: &Zone, readings: &[u8]) -> ZoneUpdate {
        if readings.len() != zone.sources.len() || readings.len() != self.source_curves.len() {
            return ZoneUpdate { duty: FULL_DUTY, curve_temps: Vec::new() };
        }

        let mut zone_readings = Vec::new();
        let mut source_temps = Vec::new();
        let mut source_duties = Vec::new();

        for ((source, held), &reading) in zone.sources.iter().zip(&mut self.source_curves).zip(readings) {
            match &source.steps {
                Some(steps) => {
                    let temp = held.update(reading, zone.hysteresis);
                    source_temps.push(temp);
                    source_duties.push(duty_for_temp(steps, temp));
                }
                None => zone_readings.push(reading),
            }
        }

        let mut curve_temps = Vec::new();
        let mut duties = Vec::new();

        if !zone_readings.is_empty() {
            let reading = aggregate(&zone_readings, &zone.aggregation, Rounding::Down);
            let temp = self.zone_curve.update(reading, zone.hysteresis);
            curve_temps.push(temp);
            // Validation leaves the zone steps absent only when no reading
            // reaches the zone curve; an absent curve maps to 100% all the same
            let steps = zone.steps.as_deref().unwrap_or(&[]);
            duties.push(duty_for_temp(steps, temp));
        }

        curve_temps.append(&mut source_temps);
        duties.append(&mut source_duties);

        // Each reading lands in the zone curve or in its own curve, and the
        // reading count equals the source count, so `duties` is empty only for
        // a zone without sources, which validation rejects. The check keeps an
        // empty slice away from `aggregate` all the same.
        let duty = if duties.is_empty() {
            FULL_DUTY
        } else {
            aggregate(&duties, &zone.duty_aggregation, Rounding::Up)
        };

        ZoneUpdate { duty, curve_temps }
    }
}

#[cfg(test)]
mod tests {
    use {
        super::{aggregate, duty_for_temp, Rounding, ZoneState},
        crate::config::{
            Aggregation,
            Interval,
            OnFailure,
            Retries,
            RetryDelayMs,
            SessionName,
            Source,
            SourceConfig,
            Step,
            Zone,
        },
    };

    const ZONE_STEPS: [Step; 2] = STEPS;
    const GPU_STEPS: [Step; 2] = [
        Step { temp: 60, dcycle: 40 },
        Step { temp: 90, dcycle: 100 },
    ];
    const HOLD_STEPS: [Step; 2] = [
        Step { temp: 40, dcycle: 40 },
        Step { temp: 80, dcycle: 100 },
    ];

    /// A source that feeds the zone curve
    fn plain() -> SourceConfig {
        SourceConfig {
            kind: Source::File { path: "/sys/temp".to_owned() },
            steps: None,
            on_failure: OnFailure::default(),
        }
    }

    /// A source with a curve of its own
    fn curved(steps: &[Step]) -> SourceConfig {
        SourceConfig { steps: Some(steps.to_vec()), ..plain() }
    }

    fn zone(steps: &[Step], sources: Vec<SourceConfig>) -> Zone {
        Zone {
            session: SessionName::default(),
            interval: Interval::default(),
            retries: Retries::default(),
            retry_delay_ms: RetryDelayMs::default(),
            ipmi_zones: vec![0],
            sources,
            aggregation: Aggregation::Maximum,
            steps: Some(steps.to_vec()),
            duty_aggregation: Aggregation::Maximum,
            hysteresis: 0,
        }
    }

    /// Feed one set of readings per poll and collect the duties
    fn duties(zone: &Zone, polls: &[&[u8]]) -> Vec<u8> {
        let mut state = ZoneState::new(zone);

        polls.iter().map(|readings| state.next_duty(zone, readings).duty).collect()
    }

    #[test]
    fn cpu_and_gpu_curves_give_the_higher_duty() {
        // CPU 50 C on the zone curve gives 54; GPU 70 C on its own gives 60
        let zone = zone(&ZONE_STEPS, vec![plain(), curved(&GPU_STEPS)]);

        assert_eq!(duties(&zone, &[&[50, 70]]), [60]);
    }

    #[test]
    fn average_duty_aggregation_averages_curve_duties() {
        let mut zone = zone(&ZONE_STEPS, vec![plain(), curved(&GPU_STEPS)]);
        zone.duty_aggregation = Aggregation::Average { top: None };

        assert_eq!(duties(&zone, &[&[50, 70]]), [57]);
    }

    #[test]
    fn average_duty_aggregation_rounds_up() {
        // Duties 99 and 100 have the mean 99.5
        let mut zone = zone(&[Step { temp: 0, dcycle: 99 }], vec![plain(), curved(&[Step { temp: 0, dcycle: 100 }])]);
        zone.duty_aggregation = Aggregation::Average { top: None };

        assert_eq!(duties(&zone, &[&[50, 50]]), [100]);
    }

    #[test]
    fn average_duty_aggregation_counts_each_curve_once() {
        // Two sources share the zone curve, which gives 100; the third source's
        // curve gives 21. The two curve duties have the mean 60.5.
        let mut zone = zone(&ZONE_STEPS, vec![plain(), plain(), curved(&[Step { temp: 0, dcycle: 21 }])]);
        zone.duty_aggregation = Aggregation::Average { top: None };

        assert_eq!(duties(&zone, &[&[80, 50, 50]]), [61]);
    }

    #[test]
    fn average_zone_temperature_truncates() {
        // Readings 50 and 51 have the mean 50.5
        let mut zone = zone(&HOLD_STEPS, vec![plain(), plain()]);
        zone.aggregation = Aggregation::Average { top: None };
        let update = ZoneState::new(&zone).next_duty(&zone, &[50, 51]);

        assert_eq!(update.curve_temps, [50]);
        assert_eq!(update.duty, 55);
    }

    #[test]
    fn hot_cpu_drives_full_duty_over_cool_gpu() {
        let zone = zone(&ZONE_STEPS, vec![plain(), curved(&GPU_STEPS)]);

        assert_eq!(duties(&zone, &[&[75, 70]]), [100]);
    }

    #[test]
    fn zone_curve_is_skipped_when_every_source_has_steps() {
        // An empty zone curve would give 100% if it were used
        let zone = zone(&[], vec![curved(&GPU_STEPS), curved(&GPU_STEPS)]);
        let update = ZoneState::new(&zone).next_duty(&zone, &[70, 60]);

        assert_eq!(update.duty, 60);
        assert_eq!(update.curve_temps, [70, 60]);
    }

    #[test]
    fn absent_zone_steps_leave_source_curves_in_charge() {
        let mut zone = zone(&[], vec![curved(&GPU_STEPS), curved(&GPU_STEPS)]);
        zone.steps = None;

        assert_eq!(duties(&zone, &[&[70, 60]]), [60]);
    }

    #[test]
    fn absent_zone_steps_give_full_duty_if_a_reading_reaches_them() {
        // Validation rejects this zone; the curve still fails toward 100%
        let mut zone = zone(&[], vec![plain()]);
        zone.steps = None;

        assert_eq!(duties(&zone, &[&[30]]), [100]);
    }

    #[test]
    fn zone_temperature_covers_only_sources_without_steps() {
        // 50 and 70 average to 60, which the zone curve maps to 72; the 90 C
        // source on its constant 40% curve must not enter the average
        let constant = [Step { temp: 0, dcycle: 40 }];
        let mut zone = zone(&ZONE_STEPS, vec![plain(), plain(), curved(&constant)]);
        zone.aggregation = Aggregation::Average { top: None };
        let update = ZoneState::new(&zone).next_duty(&zone, &[50, 70, 90]);

        assert_eq!(update.duty, 72);
        assert_eq!(update.curve_temps, [60, 90]);
    }

    #[test]
    fn curve_temps_list_zone_curve_then_sources_in_order() {
        let zone = zone(&ZONE_STEPS, vec![curved(&GPU_STEPS), plain(), curved(&HOLD_STEPS)]);
        let update = ZoneState::new(&zone).next_duty(&zone, &[61, 50, 62]);

        assert_eq!(update.curve_temps, [50, 61, 62]);
    }

    #[test]
    fn hysteresis_holds_small_falls() {
        let mut zone = zone(&HOLD_STEPS, vec![plain()]);
        zone.hysteresis = 5;

        assert_eq!(duties(&zone, &[&[60], &[58], &[54], &[61]]), [70, 70, 61, 71]);
    }

    #[test]
    fn hysteresis_holds_small_falls_on_a_source_curve() {
        let mut zone = zone(&[], vec![curved(&HOLD_STEPS)]);
        zone.hysteresis = 5;

        assert_eq!(duties(&zone, &[&[60], &[58], &[54], &[61]]), [70, 70, 61, 71]);
    }

    #[test]
    fn rise_smaller_than_hysteresis_is_followed_from_a_held_temperature() {
        let mut zone = zone(&HOLD_STEPS, vec![plain()]);
        zone.hysteresis = 5;

        assert_eq!(duties(&zone, &[&[60], &[58], &[62]]), [70, 70, 73]);
    }

    #[test]
    fn rise_smaller_than_hysteresis_is_followed() {
        let mut zone = zone(&HOLD_STEPS, vec![plain()]);
        zone.hysteresis = 5;

        assert_eq!(duties(&zone, &[&[60], &[61]]), [70, 71]);
    }

    #[test]
    fn fall_equal_to_hysteresis_is_followed() {
        let mut zone = zone(&HOLD_STEPS, vec![plain()]);
        zone.hysteresis = 5;

        assert_eq!(duties(&zone, &[&[60], &[55]]), [70, 62]);
    }

    #[test]
    fn absent_hysteresis_follows_every_reading() {
        let zone = zone(&HOLD_STEPS, vec![plain()]);

        assert_eq!(duties(&zone, &[&[60], &[58], &[54], &[61]]), [70, 67, 61, 71]);
    }

    #[test]
    fn each_curve_holds_its_own_temperature() {
        // The zone curve falls 2 and holds; the source curve falls 10 and follows
        let mut zone = zone(&HOLD_STEPS, vec![plain(), curved(&HOLD_STEPS)]);
        zone.hysteresis = 5;
        let mut state = ZoneState::new(&zone);
        state.next_duty(&zone, &[60, 60]);

        assert_eq!(state.next_duty(&zone, &[58, 50]).curve_temps, [60, 50]);
    }

    #[test]
    fn zone_without_source_steps_keeps_its_duty() {
        // A stage 1 zone: no source steps, no hysteresis, default aggregation
        let zone = zone(&ZONE_STEPS, vec![plain(), plain(), plain()]);
        let update = ZoneState::new(&zone).next_duty(&zone, &[50, 70, 60]);

        assert_eq!(update.duty, duty_for_temp(&ZONE_STEPS, 70));
        assert_eq!(update.duty, 90);
        assert_eq!(update.curve_temps, [70]);
    }

    #[test]
    fn zone_without_steps_or_source_steps_gives_full_duty() {
        let zone = zone(&[], vec![plain()]);

        assert_eq!(duties(&zone, &[&[30]]), [100]);
    }

    #[test]
    fn reading_count_mismatch_gives_full_duty() {
        let zone = zone(&ZONE_STEPS, vec![plain(), curved(&GPU_STEPS)]);

        assert_eq!(duties(&zone, &[&[30]]), [100]);
    }

    const STEPS: [Step; 2] = [
        Step { temp: 45, dcycle: 45 },
        Step { temp: 75, dcycle: 100 },
    ];

    #[test]
    fn interpolates_between_steps() {
        assert_eq!(duty_for_temp(&STEPS, 60), 72);
    }

    #[test]
    fn holds_first_step_below_range() {
        assert_eq!(duty_for_temp(&STEPS, 30), 45);
    }

    #[test]
    fn holds_last_step_above_range() {
        // The last duty differs from the 100% that an empty curve gives
        let steps = [
            Step { temp: 45, dcycle: 45 },
            Step { temp: 75, dcycle: 85 },
        ];

        assert_eq!(duty_for_temp(&steps, 90), 85);
    }

    #[test]
    fn holds_full_duty_last_step_above_range() {
        assert_eq!(duty_for_temp(&STEPS, 90), 100);
    }

    #[test]
    fn temp_equal_to_step_gives_its_dcycle() {
        assert_eq!(duty_for_temp(&STEPS, 45), 45);
        assert_eq!(duty_for_temp(&STEPS, 75), 100);
    }

    #[test]
    fn no_steps_gives_full_duty() {
        assert_eq!(duty_for_temp(&[], 50), 100);
    }

    #[test]
    fn single_step_gives_fixed_duty() {
        let steps = [Step { temp: 50, dcycle: 60 }];

        assert_eq!(duty_for_temp(&steps, 20), 60);
        assert_eq!(duty_for_temp(&steps, 50), 60);
        assert_eq!(duty_for_temp(&steps, 90), 60);
    }

    #[test]
    fn maximum_takes_highest_reading() {
        assert_eq!(aggregate(&[50, 70, 60], &Aggregation::Maximum, Rounding::Down), 70);
        assert_eq!(aggregate(&[50, 70, 60], &Aggregation::Maximum, Rounding::Up), 70);
    }

    #[test]
    fn average_with_top_takes_highest_readings() {
        let aggregation = Aggregation::Average { top: Some(2) };

        assert_eq!(aggregate(&[50, 70, 60], &aggregation, Rounding::Down), 65);
    }

    #[test]
    fn average_without_top_takes_all_readings() {
        let aggregation = Aggregation::Average { top: None };

        // Mean 56.67 truncates to 56; the median would be 60
        assert_eq!(aggregate(&[20, 90, 60], &aggregation, Rounding::Down), 56);
    }

    #[test]
    fn average_rounding_up_takes_the_next_integer() {
        let aggregation = Aggregation::Average { top: None };

        assert_eq!(aggregate(&[20, 90, 60], &aggregation, Rounding::Up), 57);
        assert_eq!(aggregate(&[99, 100], &aggregation, Rounding::Up), 100);
    }

    #[test]
    fn average_rounding_up_keeps_an_integer_mean() {
        let aggregation = Aggregation::Average { top: None };

        assert_eq!(aggregate(&[50, 70, 60], &aggregation, Rounding::Up), 60);
        assert_eq!(aggregate(&[255, 255], &aggregation, Rounding::Up), 255);
    }

    #[test]
    fn average_with_top_above_count_divides_by_readings_used() {
        let aggregation = Aggregation::Average { top: Some(3) };

        assert_eq!(aggregate(&[80, 80], &aggregation, Rounding::Down), 80);
    }
}
