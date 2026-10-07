use dng_monochrome::{
    expression::{self, Expression, FunctionPolicy},
    range::{Histogram, LEVELS},
};

fn compare(text: &str, expected: impl Fn(f64) -> f64) {
    let expression = Expression::parse(text).unwrap();
    let mut function = expression.bind();
    for i in 0..=1000 {
        let x = f64::from(i) / 1000.0;
        let (actual, expected) = (function(x), expected(x));
        assert!(
            (actual - expected).abs() <= 2e-12 * expected.abs().max(1.0),
            "{text}, x={x}: {actual} != {expected}"
        );
    }
}

#[test]
fn all_requested_simple_expressions_and_precedence() {
    compare("x^2", |x| x * x);
    compare("x^.5", f64::sqrt);
    compare("sqrt(x)", f64::sqrt);
    compare("x/2", |x| x / 2.0);
    compare("sin(x*pi)", |x| (x * std::f64::consts::PI).sin());
    compare("-x^2", |x| -(x * x));
    compare("(-x)^2", |x| x * x);
    compare("x^2^3", |x| x.powi(8));
    compare("2^-3 + +x", |x| 0.125 + x);
    compare("3-2-1+x", |x| x);
    compare("8/2/2*x", |x| 2.0 * x);
    compare("1e-3*x + 2.5E+1", |x| 0.001 * x + 25.0);
    compare("(x*7)%3", |x| (x * 7.0) % 3.0);
}

#[test]
fn real_number_conventions_match_existing_sibling_calculators() {
    compare("2+3*4", |_| 14.0);
    compare("(2+3)*4", |_| 20.0);
    compare("2^3^2", |_| 512.0);
    compare("-2^2", |_| -4.0);
    compare("6/3/2", |_| 1.0);
    compare("sin(x)^2+cos(x)^2", |_| 1.0);
    compare("ln(exp(x))", |x| x);
    compare(".5+1.5e-3+x^.5", |x| 0.5015 + x.sqrt());
}

#[test]
fn long_nested_photographic_expression_matches_independent_formula() {
    compare(
        "((exp(-((x-.37)^2)/(2*(.19^2)))*(sin(pi*(x+(.25*cos(2*pi*x))))^2))\
          +((log1p(3*x)/ln(4))*(1-(1-x)^3))\
          -.125*(cos(pi*x)-1)^2\
          +sqrt(abs(sin((pi/2)*(x^(1/2)))))*.05)\
          /(1+exp(-(8*(x-.5))))",
        |x| {
            let pi = std::f64::consts::PI;
            let gaussian = (-(x - 0.37).powi(2) / (2.0 * 0.19f64.powi(2))).exp();
            let oscillation = (pi * (x + 0.25 * (2.0 * pi * x).cos())).sin().powi(2);
            let highlight = (3.0 * x).ln_1p() / 4.0f64.ln() * (1.0 - (1.0 - x).powi(3));
            let contrast = 0.125 * ((pi * x).cos() - 1.0).powi(2);
            let lift = ((pi / 2.0) * x.sqrt()).sin().abs().sqrt() * 0.05;
            (gaussian * oscillation + highlight - contrast + lift)
                / (1.0 + (-8.0 * (x - 0.5)).exp())
        },
    );
    compare(
        "clamp(((exp(2*(x-.5))-exp(-1))/(exp(1)-exp(-1)))\
          +.15*(sin(pi*x)^2)*(1-(2*x-1)^2)\
          -(log(1+x)-x/2)/10,0,1)",
        |x| {
            (((2.0 * (x - 0.5)).exp() - (-1.0f64).exp()) / (1.0f64.exp() - (-1.0f64).exp())
                + 0.15 * (std::f64::consts::PI * x).sin().powi(2) * (1.0 - (2.0 * x - 1.0).powi(2))
                - ((1.0 + x).ln() - x / 2.0) / 10.0)
                .clamp(0.0, 1.0)
        },
    );
}

#[test]
fn thousands_of_characters_and_deep_nesting() {
    let term = "((sin(pi*x)^2+cos(pi*x)^2)*exp(ln(x+1))-x)";
    let long = format!("({})/128", vec![term; 128].join("+"));
    assert!(long.len() > 5000);
    compare(&long, |_| 1.0);
    let nested = format!("{}x{}", "(".repeat(100), ")".repeat(100));
    compare(&nested, |x| x);
}

#[test]
fn all_documented_math_functions() {
    let pi = std::f64::consts::PI;
    compare("pi+e+x", |x| pi + std::f64::consts::E + x);
    compare("sqrt(x)+abs(x-.5)+exp(x)+ln(x+1)", |x| {
        x.sqrt() + (x - 0.5).abs() + x.exp() + (x + 1.0).ln()
    });
    compare("log(x+1)+log2(x+1)+log10(x+1)+log1p(x)", |x| {
        (x + 1.0).ln() + (x + 1.0).log2() + (x + 1.0).log10() + x.ln_1p()
    });
    compare("exp2(x)+expm1(x)+pow(x,3)+hypot(x,1)", |x| {
        x.exp2() + x.exp_m1() + x.powi(3) + x.hypot(1.0)
    });
    compare("sin(x)+cos(x)+tan(x)", |x| x.sin() + x.cos() + x.tan());
    compare("asin(x)+acos(x)+atan(x)+atan2(x,1)", |x| {
        x.asin() + x.acos() + x.atan() + x.atan2(1.0)
    });
    compare("sinh(x)+cosh(x)+tanh(x)", |x| {
        x.sinh() + x.cosh() + x.tanh()
    });
    compare("asinh(x)+acosh(x+1)+atanh(x/2)", |x| {
        x.asinh() + (x + 1.0).acosh() + (x / 2.0).atanh()
    });
    compare("floor(x*3)+ceil(x*3)+round(x*3)", |x| {
        (x * 3.0).floor() + (x * 3.0).ceil() + (x * 3.0).round()
    });
    compare("sign(x-.5)+signum(x-.5)", |x| {
        let z = x - 0.5;
        (if z == 0.0 { 0.0 } else { z.signum() }) + z.signum()
    });
    compare("min(x,.2,.7)+max(x,.2,.7)+clamp(x,.2,.7)", |x| {
        x.min(0.2).min(0.7) + x.max(0.2).max(0.7) + x.clamp(0.2, 0.7)
    });
}

#[test]
fn invalid_syntax_names_arity_and_limits_are_errors() {
    for text in [
        "",
        " ",
        "x+",
        "sin(",
        "(x))",
        "x y",
        "y+x",
        "execute(x)",
        "sin()",
        "sin(x,2)",
        "pow(x)",
        "clamp(x,0)",
        "min()",
        "2x",
        "x**2",
        "π*x",
    ] {
        assert!(
            Expression::parse(text).is_err(),
            "unexpectedly accepted {text:?}"
        );
    }
    assert!(Expression::parse(&"x".repeat(16385)).is_err());
    assert!(Expression::parse(&format!("{}x{}", "(".repeat(129), ")".repeat(129))).is_err());
    assert!(Expression::parse(&vec!["x"; 300].join("^")).is_err());
    assert!(Expression::parse("1e999").is_err());
    assert!(Expression::parse("1e-").is_err());
}

#[test]
fn clip_scale_and_wrap_have_exact_boundary_semantics() {
    let hist = Histogram::new(&[0, 1, 2, 3, 4, 5]).unwrap();
    for (policy, expected) in [
        (FunctionPolicy::Clip, [0.0, 0.0, 0.5, 1.0, 1.0, 1.0]),
        (
            FunctionPolicy::Scale,
            [0.0, 1.0 / 3.0, 0.5, 2.0 / 3.0, 5.0 / 6.0, 1.0],
        ),
        (FunctionPolicy::Wrap, [0.0, 0.0, 0.5, 1.0, 0.5, 0.0]),
    ] {
        let mut values = vec![0.0; LEVELS];
        values[..6].copy_from_slice(&[-1.0, 0.0, 0.5, 1.0, 1.5, 2.0]);
        let report = expression::apply(&mut values, &hist, None, policy).unwrap();
        assert_eq!(&values[..6], &expected);
        assert_eq!(report.outside_unit_range_percent, 50.0);
    }
    let hist = Histogram::new(&[0]).unwrap();
    let mut values = vec![0.0; LEVELS];
    values[0] = -0.2;
    expression::apply(&mut values, &hist, None, FunctionPolicy::Wrap).unwrap();
    assert!((values[0] - 0.8).abs() < 1e-15);
}

#[test]
fn scaling_and_domain_checks_use_only_actual_pixels() {
    let hist = Histogram::new(&[100, 200, 200]).unwrap();
    let mut values = vec![0.0; LEVELS];
    values[100] = 0.25;
    values[200] = 0.5;
    let expression = Expression::parse("ln(x)").unwrap();
    let report =
        expression::apply(&mut values, &hist, Some(&expression), FunctionPolicy::Scale).unwrap();
    assert_eq!(values[100], 0.0);
    assert_eq!(values[200], 1.0);
    assert_eq!(report.observed_min, 0.25f64.ln());
    assert_eq!(report.observed_max, 0.5f64.ln());
}

#[test]
fn constant_and_non_finite_results_are_explicit_errors() {
    let hist = Histogram::new(&[0, 1]).unwrap();
    for source in [
        "sqrt(-1)",
        "ln(0)",
        "1/(x-x)",
        "exp(1000)",
        "(-1)^.5",
        "clamp(x,2,1)",
    ] {
        let expression = Expression::parse(source).unwrap();
        for policy in [
            FunctionPolicy::Clip,
            FunctionPolicy::Scale,
            FunctionPolicy::Wrap,
        ] {
            let mut values = vec![0.5; LEVELS];
            let error =
                expression::apply(&mut values, &hist, Some(&expression), policy).unwrap_err();
            assert!(
                error.to_string().contains("non-finite"),
                "{source}: {error}"
            );
        }
    }
    let expression = Expression::parse("0.5").unwrap();
    let mut values = vec![0.5; LEVELS];
    assert!(
        expression::apply(&mut values, &hist, Some(&expression), FunctionPolicy::Scale).is_err()
    );
    expression::apply(&mut values, &hist, Some(&expression), FunctionPolicy::Clip).unwrap();
    assert_eq!(values[0], 0.5);
}

#[test]
fn scale_handles_finite_extremes_without_overflow() {
    let hist = Histogram::new(&[0, 1, 2]).unwrap();
    let mut values = vec![0.0; LEVELS];
    values[1] = 0.5;
    values[2] = 1.0;
    let expression = Expression::parse("1e308*(2*x-1)").unwrap();
    expression::apply(&mut values, &hist, Some(&expression), FunctionPolicy::Scale).unwrap();
    assert_eq!(&values[..3], &[0.0, 0.5, 1.0]);
}

#[test]
fn arbitrary_short_input_does_not_panic() {
    let alphabet = b"xpie0123456789.+-*/%^(),!@_; \t\n";
    let mut state = 987654321u64;
    for length in 0..80 {
        for _ in 0..50 {
            let text: String = (0..length)
                .map(|_| {
                    state = state.wrapping_mul(6364136223846793005).wrapping_add(1);
                    char::from(alphabet[(state >> 32) as usize % alphabet.len()])
                })
                .collect();
            if let Ok(expression) = Expression::parse(&text) {
                let mut function = expression.bind();
                for x in [0.0, 0.5, 1.0] {
                    let _ = function(x);
                }
            }
        }
    }
}

fn reference_srgb(x: f64) -> f64 {
    if x <= 0.0031308 {
        12.92 * x
    } else {
        1.055 * x.powf(1.0 / 2.4) - 0.055
    }
}

fn reference_band(x: f64, start: f64, end: f64, amount: f64) -> f64 {
    if x <= start || x >= end || amount == 0.0 {
        return x;
    }
    let target = start + (end - start) * reference_srgb((x - start) / (end - start));
    (1.0 - amount) * x + amount * target
}

#[test]
fn selective_srgb_helpers_match_exact_transfer_and_nested_expressions() {
    compare("srgb(x)", reference_srgb);
    compare("0.6*x + 0.4*srgb(x)", |x| 0.6 * x + 0.4 * reference_srgb(x));
    compare("srgb_band(x,0,1,1)", reference_srgb);
    compare("srgb_band(x,0,1,0)", |x| x);
    compare(
        "srgb_band(srgb_band((exp(2*x)-1)/(exp(2)-1),0,.35,.4),.7,1,.2)\
                     +.01*sin(pi*x)^2",
        |x| {
            let base = ((2.0 * x).exp() - 1.0) / (2.0f64.exp() - 1.0);
            reference_band(reference_band(base, 0.0, 0.35, 0.4), 0.7, 1.0, 0.2)
                + 0.01 * (std::f64::consts::PI * x).sin().powi(2)
        },
    );
    let expression = Expression::parse("srgb(x)").unwrap();
    let mut srgb = expression.bind();
    for x in [0.0, 0.0031308, 0.0031308001, 0.018, 0.18, 1.0, -0.1, 1.5] {
        assert_eq!(srgb(x), reference_srgb(x));
    }
}

#[test]
fn selective_srgb_bands_preserve_order_endpoints_and_all_untouched_codes() {
    for (start, end) in [(0.0, 0.35), (0.7, 1.0), (0.0, 1.0), (0.2, 0.8)] {
        for amount in [0.0, 0.35, 0.4, 0.5, 1.0] {
            let source = format!("srgb_band(x,{start},{end},{amount})");
            let expression = Expression::parse(&source).unwrap();
            let mut function = expression.bind();
            let mut previous = 0.0;
            for code in 0..=65535 {
                let x = f64::from(code) / 65535.0;
                let y = function(x);
                assert!(y.is_finite() && (0.0..=1.0).contains(&y), "{source}, x={x}");
                assert!(y >= previous, "{source}, x={x}: {y} < {previous}");
                assert!((y - reference_band(x, start, end, amount)).abs() <= 2e-15);
                if x <= start || x >= end || amount == 0.0 {
                    assert_eq!(y, x, "{source} changed a protected value");
                } else {
                    assert!(y >= x && y <= end);
                }
                previous = y;
            }
            assert_eq!(function(start), start);
            assert_eq!(function(end), end);
            for x in [-1.0, 2.0] {
                assert_eq!(function(x), x);
            }
        }
    }
    let expression = Expression::parse("srgb_band(x,0,.35,.4)").unwrap();
    let mut lift = expression.bind();
    assert!((lift(0.05) - 0.08795326244941004).abs() < 1e-15);
    assert!((lift(0.1) - 0.13993659127375752).abs() < 1e-15);
}

#[test]
fn selective_srgb_invalid_parameters_and_arity_are_explicit_errors() {
    for source in [
        "srgb()",
        "srgb(x,1)",
        "srgb_band(x)",
        "srgb_band(x,0,1)",
        "srgb_band(x,0,1,.5,2)",
    ] {
        assert!(Expression::parse(source).is_err(), "{source}");
    }
    let hist = Histogram::new(&[0]).unwrap();
    for source in [
        "srgb_band(x,-.1,.5,.4)",
        "srgb_band(x,0,1.1,.4)",
        "srgb_band(x,.5,.5,.4)",
        "srgb_band(x,.7,.3,.4)",
        "srgb_band(x,0,.5,-.1)",
        "srgb_band(x,0,.5,1.1)",
        "srgb_band(x,0,exp(1000),0)",
        "srgb_band(x,0,1,0/0)",
        "srgb_band(0/0,0,1,0)",
    ] {
        let expression = Expression::parse(source).unwrap();
        for policy in [
            FunctionPolicy::Clip,
            FunctionPolicy::Scale,
            FunctionPolicy::Wrap,
        ] {
            let mut values = vec![0.8; LEVELS];
            let error =
                expression::apply(&mut values, &hist, Some(&expression), policy).unwrap_err();
            assert!(
                error.to_string().contains("non-finite"),
                "{source}: {error}"
            );
        }
    }
}
