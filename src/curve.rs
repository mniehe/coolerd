use {
    std::cmp::Reverse,
    crate::config::{Aggregation, Step},
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

/// Combine the source readings into one temperature using the zone's data
/// aggregation method
pub fn aggregate(readings: &[u8], aggregation: &Aggregation) -> u8 {
    let mut readings = readings.to_vec();
    readings.sort_by_key(|r| Reverse(*r));

    // The source list is guaranteed to never be empty so if no error
    // occurs, there will always be an equal number of readings
    match aggregation {
        Aggregation::Maximum => {
            readings.first().copied().unwrap()
        }
        Aggregation::Average { top } => {
            let n = top.unwrap_or(readings.len());

            let sum = readings
                .into_iter()
                .take(n)
                .map(u32::from)
                .sum::<u32>();

            (sum as f32 / n as f32) as u8
        }
    }
}

#[cfg(test)]
mod tests {
    use {
        super::{aggregate, duty_for_temp},
        crate::config::{Aggregation, Step},
    };

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
        assert_eq!(aggregate(&[50, 70, 60], &Aggregation::Maximum), 70);
    }

    #[test]
    fn average_with_top_takes_highest_readings() {
        let aggregation = Aggregation::Average { top: Some(2) };

        assert_eq!(aggregate(&[50, 70, 60], &aggregation), 65);
    }

    #[test]
    fn average_without_top_takes_all_readings() {
        let aggregation = Aggregation::Average { top: None };

        assert_eq!(aggregate(&[50, 70, 60], &aggregation), 60);
    }

    // Pins today's result: the sum of the readings present is divided by
    // `top`, not by the number of readings.
    #[test]
    fn average_with_top_above_count_divides_by_top() {
        let aggregation = Aggregation::Average { top: Some(3) };

        assert_eq!(aggregate(&[80, 80], &aggregation), 53);
    }
}
