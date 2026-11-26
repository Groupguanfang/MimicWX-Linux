#[derive(Debug, PartialEq, Eq)]
pub(super) struct MotionStep {
    pub x: i16,
    pub y: i16,
    pub at_ms: u64,
}

pub(super) fn plan_motion(
    start: (i16, i16),
    target: (i16, i16),
    screen: (u16, u16),
    variation: [u8; 2],
) -> Vec<MotionStep> {
    let delta_x = f64::from(target.0) - f64::from(start.0);
    let delta_y = f64::from(target.1) - f64::from(start.1);
    let distance = delta_x.hypot(delta_y);
    if distance == 0.0 {
        return Vec::new();
    }

    let duration_ms = if distance <= 4.0 {
        (distance * 8.0).round() as u64
    } else {
        let speed_variation = 0.9 + f64::from(variation[0]) / 255.0 * 0.2;
        ((55.0 + distance.sqrt() * 10.5) * speed_variation)
            .round()
            .clamp(64.0, 420.0) as u64
    };
    let frame_count = duration_ms.div_ceil(10);
    let bend = (f64::from(variation[1]) / 127.5 - 1.0) * (distance * 0.08).min(32.0);
    let max_x = f64::from(screen.0.saturating_sub(1).min(i16::MAX as u16));
    let max_y = f64::from(screen.1.saturating_sub(1).min(i16::MAX as u16));
    let mut steps = Vec::with_capacity(frame_count as usize);
    let mut previous = start;

    for frame in 1..=frame_count {
        let fraction = frame as f64 / frame_count as f64;
        let progress = fraction.powi(3) * (10.0 + fraction * (-15.0 + fraction * 6.0));
        let arc = 2.0 * (1.0 - progress) * progress * bend;
        let position = if frame == frame_count {
            target
        } else {
            (
                (f64::from(start.0) + delta_x * progress - delta_y / distance * arc)
                    .round()
                    .clamp(0.0, max_x) as i16,
                (f64::from(start.1) + delta_y * progress + delta_x / distance * arc)
                    .round()
                    .clamp(0.0, max_y) as i16,
            )
        };
        if position == previous && frame != frame_count {
            continue;
        }
        steps.push(MotionStep {
            x: position.0,
            y: position.1,
            at_ms: duration_ms * frame / frame_count,
        });
        previous = position;
    }

    steps
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stationary_pointer_needs_no_events_or_delay() {
        assert!(plan_motion((120, 80), (120, 80), (1280, 800), [128; 2]).is_empty());
    }

    #[test]
    fn nearby_targets_have_short_movements() {
        for target in [(101, 100), (100, 99), (102, 102), (96, 100)] {
            let steps = plan_motion((100, 100), target, (1280, 800), [128; 2]);
            let last = steps.last().unwrap();
            assert_eq!((last.x, last.y), target);
            assert!(last.at_ms <= 32);
            assert!(steps.len() <= 4);
        }
    }

    #[test]
    fn long_movements_accelerate_and_decelerate() {
        let steps = plan_motion((50, 100), (1050, 100), (1280, 800), [128; 2]);
        let duration = steps.last().unwrap().at_ms;
        let first_quarter = steps
            .iter()
            .find(|step| step.at_ms >= duration / 4)
            .unwrap();
        let last_quarter = steps
            .iter()
            .find(|step| step.at_ms >= duration * 3 / 4)
            .unwrap();
        let middle_distance = last_quarter.x - first_quarter.x;
        assert!(middle_distance > (first_quarter.x - 50) * 2);
        assert!(middle_distance > (1050 - last_quarter.x) * 2);
        assert!(steps.len() >= 20);
        assert!(steps.windows(2).all(|pair| {
            pair[0].at_ms < pair[1].at_ms && pair[1].x >= pair[0].x && pair[1].x - pair[0].x < 100
        }));
    }

    #[test]
    fn curves_vary_without_changing_the_destination() {
        let left = plan_motion((100, 300), (900, 300), (1280, 800), [128, 0]);
        let right = plan_motion((100, 300), (900, 300), (1280, 800), [128, 255]);
        assert!(left.iter().any(|step| step.y < 300));
        assert!(right.iter().any(|step| step.y > 300));
        assert!(left.iter().all(|step| (step.y - 300).abs() <= 16));
        assert!(right.iter().all(|step| (step.y - 300).abs() <= 16));
        assert_eq!(left.last(), right.last());
        assert_ne!(
            plan_motion((100, 300), (900, 300), (1280, 800), [0, 0])
                .last()
                .unwrap()
                .at_ms,
            plan_motion((100, 300), (900, 300), (1280, 800), [255, 0])
                .last()
                .unwrap()
                .at_ms
        );
    }

    #[test]
    fn screen_edges_and_long_distances_stay_bounded() {
        for screen in [(2, 2), (1280, 800), (32768, 32768)] {
            let max_x = (screen.0 - 1) as i16;
            let max_y = (screen.1 - 1) as i16;
            let corners = [(0, 0), (max_x, 0), (0, max_y), (max_x, max_y)];
            for start in corners {
                for target in corners {
                    if start == target {
                        continue;
                    }
                    for variation in [[0; 2], [128; 2], [255; 2]] {
                        let steps = plan_motion(start, target, screen, variation);
                        let last = steps.last().unwrap();
                        assert_eq!((last.x, last.y), target);
                        assert!(last.at_ms <= 420);
                        assert!(steps.len() <= 42);
                        assert!(steps.iter().all(|step| {
                            (0..=max_x).contains(&step.x) && (0..=max_y).contains(&step.y)
                        }));
                        assert!(steps.windows(2).all(|pair| pair[0].at_ms < pair[1].at_ms));
                    }
                }
            }
        }
    }
}
