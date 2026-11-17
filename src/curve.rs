use {
    std::cmp::Reverse,
    crate::config::{Aggregation, OnFailure, Step, Zone},
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
/// average divides by the number of readings it used. Returns `None` when no
/// reading is selected.
pub fn aggregate(readings: &[u8], aggregation: &Aggregation, rounding: Rounding) -> Option<u8> {
    let mut readings = readings.to_vec();
    readings.sort_by_key(|r| Reverse(*r));

    match aggregation {
        Aggregation::Maximum => {
            readings.first().copied()
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

            // A mean of u8 values fits in a u8; no reading gives no mean
            dividend.checked_div(count).map(|mean| mean as u8)
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

/// The outcome of reading one source in one poll
#[derive(Clone, Debug, PartialEq)]
pub enum SourceReading {
    Temp(u8),
    /// The source failed every attempt; the string is the error text
    Failed(String),
}

/// A change in whether a source fails, named by the source's index
#[derive(Debug, PartialEq)]
pub enum Transition {
    Failed { source: usize, error: String },
    Recovered { source: usize },
}

pub struct ZoneUpdate {
    /// The duty cycle to set for the zone
    pub duty: u8,
    /// The temperature each curve mapped: the zone curve first when it is
    /// used, then the curve of each source with `steps`, in source order
    pub curve_temps: Vec<u8>,
    /// The sources that started or stopped failing in this poll
    pub transitions: Vec<Transition>,
}

/// What a zone keeps of one source between polls
struct SourceState {
    /// Only a source with `steps` uses its curve
    curve: HeldTemp,
    failed: bool,
}

impl SourceState {
    /// Record whether the source at `index` failed in this poll and return
    /// the change, if there is one
    fn transition(&mut self, index: usize, reading: &SourceReading) -> Option<Transition> {
        let failed = matches!(reading, SourceReading::Failed(_));

        if failed == self.failed {
            return None;
        }

        self.failed = failed;

        Some(match reading {
            SourceReading::Failed(error) => Transition::Failed { source: index, error: error.clone() },
            SourceReading::Temp(_) => Transition::Recovered { source: index },
        })
    }
}

/// The lowest duty the zone may run at while a source with `on_failure` fails
fn failure_floor(on_failure: &OnFailure) -> u8 {
    match on_failure {
        OnFailure::Ignore {} => 0,
        OnFailure::Dcycle { dcycle } => *dcycle,
        // An `exit` source stops the daemon instead of giving a failed
        // reading; should one arrive, the fans run at full speed
        OnFailure::Exit {} => FULL_DUTY,
    }
}

/// The held temperatures of a zone's curves and the failed sources between
/// polls
pub struct ZoneState {
    zone_curve: HeldTemp,
    /// One per source, in source order
    sources: Vec<SourceState>,
}

impl ZoneState {
    pub fn new(zone: &Zone) -> Self {
        Self {
            zone_curve: HeldTemp(None),
            sources: zone.sources.iter()
                .map(|_| SourceState { curve: HeldTemp(None), failed: false })
                .collect(),
        }
    }

    /// Whether each source, in source order, failed in the last poll
    pub fn failed_sources(&self) -> Vec<bool> {
        self.sources.iter().map(|s| s.failed).collect()
    }

    /// Compute the zone duty from one reading per source, in source order.
    /// Sources without `steps` are aggregated into the zone curve; each source
    /// with `steps` maps its reading through its own curve. A failed source
    /// takes part in neither, its curve holds its temperature, and its
    /// `on_failure` sets the lowest duty the zone may run at.
    pub fn next_duty(&mut self, zone: &Zone, readings: &[SourceReading]) -> ZoneUpdate {
        if readings.len() != zone.sources.len() || readings.len() != self.sources.len() {
            return ZoneUpdate { duty: FULL_DUTY, curve_temps: Vec::new(), transitions: Vec::new() };
        }

        let mut zone_readings = Vec::new();
        let mut source_temps = Vec::new();
        let mut source_duties = Vec::new();
        let mut transitions = Vec::new();
        let mut floor = 0;

        let sources = zone.sources.iter().zip(&mut self.sources).zip(readings);

        for (index, ((source, state), reading)) in sources.enumerate() {
            transitions.extend(state.transition(index, reading));

            let reading = match reading {
                SourceReading::Temp(temp) => *temp,
                SourceReading::Failed(_) => {
                    floor = floor.max(failure_floor(&source.on_failure));
                    continue;
                }
            };

            match &source.steps {
                Some(steps) => {
                    let temp = state.curve.update(reading, zone.hysteresis);
                    source_temps.push(temp);
                    source_duties.push(duty_for_temp(steps, temp));
                }
                None => zone_readings.push(reading),
            }
        }

        let mut curve_temps = Vec::new();
        let mut duties = Vec::new();

        // No reading reaches the zone curve when every source without `steps`
        // failed or every source has `steps`
        if let Some(reading) = aggregate(&zone_readings, &zone.aggregation, Rounding::Down) {
            let temp = self.zone_curve.update(reading, zone.hysteresis);
            curve_temps.push(temp);
            // Validation leaves the zone steps absent only when no reading
            // reaches the zone curve; an absent curve maps to 100% all the same
            let steps = zone.steps.as_deref().unwrap_or(&[]);
            duties.push(duty_for_temp(steps, temp));
        }

        curve_temps.append(&mut source_temps);
        duties.append(&mut source_duties);

        // A zone with no curve left to give a duty, as when every source
        // failed, runs at full speed
        let duty = match aggregate(&duties, &zone.duty_aggregation, Rounding::Up) {
            Some(duty) => duty.max(floor),
            None => FULL_DUTY,
        };

        ZoneUpdate { duty, curve_temps, transitions }
    }
}

#[cfg(test)]
mod tests {
    use {
        super::{
            aggregate,
            duty_for_temp,
            Rounding,
            SourceReading::{self, Failed, Temp},
            Transition,
            ZoneState,
            ZoneUpdate,
        },
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

    /// A source that runs the zone at `on_failure` while it fails
    fn failing(source: SourceConfig, on_failure: OnFailure) -> SourceConfig {
        SourceConfig { on_failure, ..source }
    }

    fn temps(values: &[u8]) -> Vec<SourceReading> {
        values.iter().map(|&t| Temp(t)).collect()
    }

    fn failed() -> SourceReading {
        Failed("timed out".to_owned())
    }

    /// Feed one set of readings per poll and collect the duties
    fn duties(zone: &Zone, polls: &[&[u8]]) -> Vec<u8> {
        let mut state = ZoneState::new(zone);

        polls.iter().map(|readings| state.next_duty(zone, &temps(readings)).duty).collect()
    }

    /// Feed one set of source readings per poll and collect each update
    fn updates(zone: &Zone, polls: &[Vec<SourceReading>]) -> Vec<ZoneUpdate> {
        let mut state = ZoneState::new(zone);

        polls.iter().map(|readings| state.next_duty(zone, readings)).collect()
    }

    const GPU_FLOOR: OnFailure = OnFailure::Dcycle { dcycle: 80 };

    /// CPU on the zone curve, GPU on its own curve
    fn cpu_gpu_zone(gpu_on_failure: OnFailure) -> Zone {
        zone(&ZONE_STEPS, vec![plain(), failing(curved(&GPU_STEPS), gpu_on_failure)])
    }

    #[test]
    fn failed_dcycle_source_raises_duty_to_its_floor() {
        // CPU 50 C alone gives 54
        let zone = cpu_gpu_zone(GPU_FLOOR);

        assert_eq!(updates(&zone, &[vec![Temp(50), failed()]])[0].duty, 80);
    }

    #[test]
    fn failed_ignore_source_is_left_out() {
        let zone = cpu_gpu_zone(OnFailure::Ignore {});
        let update = &updates(&zone, &[vec![Temp(50), failed()]])[0];

        assert_eq!(update.duty, 54);
        assert_eq!(update.curve_temps, [50]);
    }

    #[test]
    fn live_duty_above_failed_floor_is_kept() {
        let zone = cpu_gpu_zone(GPU_FLOOR);

        assert_eq!(updates(&zone, &[vec![Temp(75), failed()]])[0].duty, 100);
    }

    #[test]
    fn average_of_live_curves_above_failed_floor_is_kept() {
        let mut zone = zone(&ZONE_STEPS, vec![
            curved(&[Step { temp: 0, dcycle: 40 }]),
            curved(&[Step { temp: 0, dcycle: 60 }]),
            failing(plain(), OnFailure::Dcycle { dcycle: 30 }),
        ]);
        zone.duty_aggregation = Aggregation::Average { top: None };

        assert_eq!(updates(&zone, &[vec![Temp(50), Temp(50), failed()]])[0].duty, 50);
    }

    #[test]
    fn failed_source_leaves_zone_temperature() {
        let mut zone = zone(&ZONE_STEPS, vec![plain(), failing(plain(), OnFailure::Ignore {})]);
        zone.aggregation = Aggregation::Average { top: None };
        let update = &updates(&zone, &[vec![Temp(60), failed()]])[0];

        assert_eq!(update.curve_temps, [60]);
        assert_eq!(update.duty, 72);
    }

    #[test]
    fn all_sources_failed_with_ignore_give_full_duty() {
        let zone = zone(&ZONE_STEPS, vec![
            failing(plain(), OnFailure::Ignore {}),
            failing(curved(&GPU_STEPS), OnFailure::Ignore {}),
        ]);
        let update = &updates(&zone, &[vec![failed(), failed()]])[0];

        assert_eq!(update.duty, 100);
        assert!(update.curve_temps.is_empty());
    }

    #[test]
    fn all_sources_failed_with_a_floor_give_full_duty() {
        let zone = zone(&ZONE_STEPS, vec![
            failing(plain(), OnFailure::Dcycle { dcycle: 30 }),
            failing(curved(&GPU_STEPS), OnFailure::Ignore {}),
        ]);

        assert_eq!(updates(&zone, &[vec![failed(), failed()]])[0].duty, 100);
    }

    #[test]
    fn failed_exit_source_gives_full_duty() {
        // read_zone_sources returns the error of an `exit` source instead
        let zone = cpu_gpu_zone(OnFailure::Exit {});

        assert_eq!(updates(&zone, &[vec![Temp(50), failed()]])[0].duty, 100);
    }

    #[test]
    fn run_of_failures_gives_one_failed_transition() {
        let zone = cpu_gpu_zone(GPU_FLOOR);
        let updates = updates(&zone, &[
            vec![Temp(50), Temp(70)],
            vec![Temp(50), failed()],
            vec![Temp(50), failed()],
            vec![Temp(50), failed()],
        ]);
        let transitions: Vec<_> = updates.iter().map(|u| &u.transitions).collect();

        assert!(transitions[0].is_empty());
        assert_eq!(transitions[1], &[Transition::Failed { source: 1, error: "timed out".to_owned() }]);
        assert!(transitions[2].is_empty());
        assert!(transitions[3].is_empty());
    }

    #[test]
    fn source_failing_on_first_poll_gives_failed_transition() {
        let zone = cpu_gpu_zone(OnFailure::Ignore {});

        assert_eq!(updates(&zone, &[vec![Temp(50), failed()]])[0].transitions,
            [Transition::Failed { source: 1, error: "timed out".to_owned() }]);
    }

    #[test]
    fn recovered_source_sets_duty_again_with_one_recovery() {
        let zone = cpu_gpu_zone(GPU_FLOOR);
        let updates = updates(&zone, &[
            vec![Temp(50), failed()],
            vec![Temp(50), Temp(70)],
            vec![Temp(50), Temp(70)],
        ]);

        // CPU 50 C gives 54 and GPU 70 C gives 60
        assert_eq!(updates[0].duty, 80);
        assert_eq!(updates[1].duty, 60);
        assert_eq!(updates[1].curve_temps, [50, 70]);
        assert_eq!(updates[1].transitions, [Transition::Recovered { source: 1 }]);
        assert!(updates[2].transitions.is_empty());
    }

    #[test]
    fn failed_source_keeps_its_held_temperature() {
        let mut zone = cpu_gpu_zone(OnFailure::Ignore {});
        zone.hysteresis = 5;
        let updates = updates(&zone, &[
            vec![Temp(50), Temp(80)],
            vec![Temp(50), failed()],
            vec![Temp(50), Temp(78)],
        ]);

        // While the GPU fails, CPU 50 C alone gives 54
        assert_eq!(updates[1].duty, 54);
        // A fall of 2 from the 80 C held before the failure is held, and 80 C
        // on the GPU curve gives 80
        assert_eq!(updates[2].curve_temps, [50, 80]);
        assert_eq!(updates[2].duty, 80);
    }

    #[test]
    fn zone_curve_keeps_its_held_temperature_while_every_plain_source_fails() {
        let mut zone = zone(&HOLD_STEPS, vec![failing(plain(), OnFailure::Ignore {}), curved(&GPU_STEPS)]);
        zone.hysteresis = 5;
        let updates = updates(&zone, &[
            vec![Temp(60), Temp(70)],
            vec![failed(), Temp(70)],
            vec![Temp(58), Temp(70)],
        ]);

        // Only the GPU curve gives a duty while the zone curve has no reading
        assert_eq!(updates[1].curve_temps, [70]);
        // A fall of 2 from the 60 C held before the failure is held: 60 C on
        // the zone curve gives 70, above the GPU curve's 60
        assert_eq!(updates[2].curve_temps, [60, 70]);
        assert_eq!(updates[2].duty, 70);
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
        let update = ZoneState::new(&zone).next_duty(&zone, &temps(&[50, 51]));

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
        let update = ZoneState::new(&zone).next_duty(&zone, &temps(&[70, 60]));

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
        let update = ZoneState::new(&zone).next_duty(&zone, &temps(&[50, 70, 90]));

        assert_eq!(update.duty, 72);
        assert_eq!(update.curve_temps, [60, 90]);
    }

    #[test]
    fn curve_temps_list_zone_curve_then_sources_in_order() {
        let zone = zone(&ZONE_STEPS, vec![curved(&GPU_STEPS), plain(), curved(&HOLD_STEPS)]);
        let update = ZoneState::new(&zone).next_duty(&zone, &temps(&[61, 50, 62]));

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
        state.next_duty(&zone, &temps(&[60, 60]));

        assert_eq!(state.next_duty(&zone, &temps(&[58, 50])).curve_temps, [60, 50]);
    }

    #[test]
    fn zone_without_source_steps_keeps_its_duty() {
        // A stage 1 zone: no source steps, no hysteresis, default aggregation
        let zone = zone(&ZONE_STEPS, vec![plain(), plain(), plain()]);
        let update = ZoneState::new(&zone).next_duty(&zone, &temps(&[50, 70, 60]));

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
        assert_eq!(aggregate(&[50, 70, 60], &Aggregation::Maximum, Rounding::Down), Some(70));
        assert_eq!(aggregate(&[50, 70, 60], &Aggregation::Maximum, Rounding::Up), Some(70));
    }

    #[test]
    fn average_with_top_takes_highest_readings() {
        let aggregation = Aggregation::Average { top: Some(2) };

        assert_eq!(aggregate(&[50, 70, 60], &aggregation, Rounding::Down), Some(65));
    }

    #[test]
    fn average_without_top_takes_all_readings() {
        let aggregation = Aggregation::Average { top: None };

        // Mean 56.67 truncates to 56; the median would be 60
        assert_eq!(aggregate(&[20, 90, 60], &aggregation, Rounding::Down), Some(56));
    }

    #[test]
    fn average_rounding_up_takes_the_next_integer() {
        let aggregation = Aggregation::Average { top: None };

        assert_eq!(aggregate(&[20, 90, 60], &aggregation, Rounding::Up), Some(57));
        assert_eq!(aggregate(&[99, 100], &aggregation, Rounding::Up), Some(100));
    }

    #[test]
    fn average_rounding_up_keeps_an_integer_mean() {
        let aggregation = Aggregation::Average { top: None };

        assert_eq!(aggregate(&[50, 70, 60], &aggregation, Rounding::Up), Some(60));
        assert_eq!(aggregate(&[255, 255], &aggregation, Rounding::Up), Some(255));
    }

    #[test]
    fn empty_readings_give_no_value() {
        for aggregation in [Aggregation::Average { top: None }, Aggregation::Average { top: Some(2) }, Aggregation::Maximum] {
            for rounding in [Rounding::Down, Rounding::Up] {
                let value = aggregate(&[], &aggregation, rounding);

                assert_eq!(value, None, "{:?}", aggregation);
            }
        }
    }

    #[test]
    fn average_with_top_above_count_divides_by_readings_used() {
        let aggregation = Aggregation::Average { top: Some(3) };

        assert_eq!(aggregate(&[80, 80], &aggregation, Rounding::Down), Some(80));
    }
}
