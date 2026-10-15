use super::*;

#[test]
fn confidence_is_finite_bounded_and_optional() {
    for probability in [None, Some(0.0), Some(1.0), Some(0.5)] {
        assert!(
            Answer {
                choice: "defer".into(),
                probability
            }
            .valid_probability()
        );
    }
    for probability in [f64::NAN, f64::INFINITY, -0.01, 1.01] {
        assert!(
            !Answer {
                choice: "defer".into(),
                probability: Some(probability)
            }
            .valid_probability()
        );
    }
}
