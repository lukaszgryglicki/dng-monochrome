use crate::{
    parameters::Parameters,
    range::{Histogram, LEVELS},
};
use anyhow::{Context, Result, bail, ensure};
use serde::Serialize;

#[derive(Clone, Copy, Debug)]
pub struct ToneBand {
    start: f64,
    end: f64,
    amount: f64,
}

impl ToneBand {
    fn valid(&self) -> bool {
        [self.start, self.end, self.amount]
            .iter()
            .all(|value| value.is_finite() && (0.0..=1.0).contains(value))
            && self.start < self.end
    }

    fn map(&self, x: f64) -> f64 {
        if !self.valid() || !x.is_finite() {
            return f64::NAN;
        }
        if self.amount == 0.0 || x <= self.start || x >= self.end {
            return x;
        }
        let width = self.end - self.start;
        let target = self.start + width * crate::tone::srgb_encode((x - self.start) / width);
        ((1.0 - self.amount) * x + self.amount * target).clamp(x, self.end)
    }

    pub fn expression(bands: &[Self]) -> Result<String> {
        let mut ordered: Vec<_> = bands.iter().collect();
        ordered.sort_by(|a, b| a.start.total_cmp(&b.start));
        ensure!(
            ordered.windows(2).all(|pair| pair[0].end <= pair[1].start),
            "--tone-band ranges must not overlap; use nested srgb_band calls in --func for explicit composition"
        );
        let mut source = "srgb_band(".repeat(bands.len());
        source.push('x');
        for band in bands {
            source.push_str(&format!(",{},{},{})", band.start, band.end, band.amount));
        }
        Ok(source)
    }
}

impl std::str::FromStr for ToneBand {
    type Err = String;

    fn from_str(source: &str) -> Result<Self, Self::Err> {
        let fields: Vec<_> = source.split(':').collect();
        if fields.len() != 3 {
            return Err("expected START:END:AMOUNT, e.g. 0:0.35:0.4".into());
        }
        let parse = |index: usize| {
            fields[index].trim().parse::<f64>().map_err(|_| {
                format!(
                    "invalid tone-band value {:?}; expected a number",
                    fields[index]
                )
            })
        };
        let band = Self {
            start: parse(0)?,
            end: parse(1)?,
            amount: parse(2)?,
        };
        if !band.valid() {
            return Err(
                "tone band requires finite 0 <= START < END <= 1 and 0 <= AMOUNT <= 1".into(),
            );
        }
        Ok(band)
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum FunctionPolicy {
    #[default]
    Clip,
    Scale,
    Wrap,
}

#[derive(Debug)]
pub struct Expression {
    source: String,
    program: Vec<Instruction>,
    stack_size: usize,
}

impl Expression {
    pub fn parse(source: &str) -> Result<Self> {
        Self::parse_with_parameters(source, &Parameters::default())
    }

    pub fn parse_with_parameters(source: &str, parameters: &Parameters) -> Result<Self> {
        parameters.validate()?;
        ensure!(
            source.len() <= parameters.expression_max_bytes,
            "expression exceeds {} bytes",
            parameters.expression_max_bytes
        );
        let mut depth = 0usize;
        for ch in source.chars() {
            match ch {
                '(' => {
                    depth += 1;
                    ensure!(
                        depth <= parameters.expression_max_parentheses,
                        "expression exceeds {} nested parentheses",
                        parameters.expression_max_parentheses
                    );
                }
                ')' => depth = depth.saturating_sub(1),
                _ => {}
            }
        }
        let mut parser = Parser {
            source: source.as_bytes(),
            position: 0,
            token: Token::End,
            program: Vec::new(),
            max_depth: parameters.expression_max_depth,
        };
        parser.advance()?;
        parser.expression(0, 0)?;
        ensure!(
            matches!(parser.token, Token::End),
            "unexpected token at byte {}",
            parser.position
        );
        let mut stack_size = 0;
        let mut depth = 0usize;
        for instruction in &parser.program {
            match instruction {
                Instruction::Value(_) | Instruction::X => depth += 1,
                Instruction::Call(_, arity) => {
                    ensure!(depth >= *arity, "invalid expression stack");
                    depth = depth - arity + 1;
                }
            }
            stack_size = stack_size.max(depth);
        }
        ensure!(depth == 1, "expected a mathematical expression");
        Ok(Self {
            source: source.into(),
            program: parser.program,
            stack_size,
        })
    }

    pub fn bind(&self) -> impl FnMut(f64) -> f64 + '_ {
        let mut stack = Vec::with_capacity(self.stack_size);
        move |x| {
            stack.clear();
            for instruction in &self.program {
                match instruction {
                    Instruction::Value(value) => stack.push(*value),
                    Instruction::X => stack.push(x),
                    Instruction::Call(function, arity) => {
                        let start = stack.len() - arity;
                        let value = function.apply(&stack[start..]);
                        stack.truncate(start);
                        stack.push(value);
                    }
                }
            }
            stack[0]
        }
    }
}

#[derive(Debug)]
enum Instruction {
    Value(f64),
    X,
    Call(Function, usize),
}

#[derive(Clone, Copy, Debug)]
enum Function {
    Unary(fn(f64) -> f64),
    Binary(fn(f64, f64) -> f64),
    SrgbBand,
    Clamp,
    Min,
    Max,
}

impl Function {
    fn named(name: &str) -> Result<Self> {
        Ok(match name {
            "srgb" => Self::Unary(crate::tone::srgb_encode),
            "srgb_band" => Self::SrgbBand,
            "sqrt" => Self::Unary(f64::sqrt),
            "abs" => Self::Unary(f64::abs),
            "exp" => Self::Unary(f64::exp),
            "ln" | "log" => Self::Unary(f64::ln),
            "log2" => Self::Unary(f64::log2),
            "log10" => Self::Unary(f64::log10),
            "log1p" => Self::Unary(f64::ln_1p),
            "exp2" => Self::Unary(f64::exp2),
            "expm1" => Self::Unary(f64::exp_m1),
            "sin" => Self::Unary(f64::sin),
            "cos" => Self::Unary(f64::cos),
            "tan" => Self::Unary(f64::tan),
            "asin" => Self::Unary(f64::asin),
            "acos" => Self::Unary(f64::acos),
            "atan" => Self::Unary(f64::atan),
            "sinh" => Self::Unary(f64::sinh),
            "cosh" => Self::Unary(f64::cosh),
            "tanh" => Self::Unary(f64::tanh),
            "asinh" => Self::Unary(f64::asinh),
            "acosh" => Self::Unary(f64::acosh),
            "atanh" => Self::Unary(f64::atanh),
            "floor" => Self::Unary(f64::floor),
            "ceil" => Self::Unary(f64::ceil),
            "round" => Self::Unary(f64::round),
            "signum" => Self::Unary(f64::signum),
            "sign" => Self::Unary(|x| if x == 0.0 { 0.0 } else { x.signum() }),
            "atan2" => Self::Binary(f64::atan2),
            "pow" => Self::Binary(f64::powf),
            "hypot" => Self::Binary(f64::hypot),
            "clamp" => Self::Clamp,
            "min" => Self::Min,
            "max" => Self::Max,
            _ => bail!("unknown function {name:?}"),
        })
    }

    fn accepts(self, count: usize) -> bool {
        match self {
            Self::Unary(_) => count == 1,
            Self::Binary(_) => count == 2,
            Self::SrgbBand => count == 4,
            Self::Clamp => count == 3,
            Self::Min | Self::Max => count >= 1,
        }
    }

    fn apply(self, args: &[f64]) -> f64 {
        match self {
            Self::Unary(f) => f(args[0]),
            Self::Binary(f) => f(args[0], args[1]),
            Self::SrgbBand => ToneBand {
                start: args[1],
                end: args[2],
                amount: args[3],
            }
            .map(args[0]),
            Self::Clamp => {
                if args[1].is_finite() && args[2].is_finite() && args[1] <= args[2] {
                    args[0].clamp(args[1], args[2])
                } else {
                    f64::NAN
                }
            }
            Self::Min => args.iter().copied().fold(f64::INFINITY, f64::min),
            Self::Max => args.iter().copied().fold(f64::NEG_INFINITY, f64::max),
        }
    }
}

#[derive(Debug)]
enum Token {
    Number(f64),
    Name(String),
    Symbol(u8),
    End,
}

struct Parser<'a> {
    source: &'a [u8],
    position: usize,
    token: Token,
    program: Vec<Instruction>,
    max_depth: usize,
}

impl Parser<'_> {
    fn advance(&mut self) -> Result<Token> {
        while self
            .source
            .get(self.position)
            .is_some_and(u8::is_ascii_whitespace)
        {
            self.position += 1;
        }
        let start = self.position;
        let next = match self.source.get(self.position).copied() {
            None => Token::End,
            Some(ch) if ch.is_ascii_digit() || ch == b'.' => {
                while self
                    .source
                    .get(self.position)
                    .is_some_and(u8::is_ascii_digit)
                {
                    self.position += 1;
                }
                if self.source.get(self.position) == Some(&b'.') {
                    self.position += 1;
                    while self
                        .source
                        .get(self.position)
                        .is_some_and(u8::is_ascii_digit)
                    {
                        self.position += 1;
                    }
                }
                if matches!(self.source.get(self.position), Some(b'e' | b'E')) {
                    self.position += 1;
                    if matches!(self.source.get(self.position), Some(b'+' | b'-')) {
                        self.position += 1;
                    }
                    while self
                        .source
                        .get(self.position)
                        .is_some_and(u8::is_ascii_digit)
                    {
                        self.position += 1;
                    }
                }
                let number = std::str::from_utf8(&self.source[start..self.position])?
                    .parse::<f64>()
                    .with_context(|| format!("invalid number at byte {start}"))?;
                ensure!(
                    number.is_finite(),
                    "non-finite numeric literal at byte {start}"
                );
                Token::Number(number)
            }
            Some(ch) if ch.is_ascii_alphabetic() || ch == b'_' => {
                self.position += 1;
                while self
                    .source
                    .get(self.position)
                    .is_some_and(|ch| ch.is_ascii_alphanumeric() || *ch == b'_')
                {
                    self.position += 1;
                }
                Token::Name(std::str::from_utf8(&self.source[start..self.position])?.into())
            }
            Some(ch) if b"+-*/%^(),".contains(&ch) => {
                self.position += 1;
                Token::Symbol(ch)
            }
            Some(_) => bail!("unexpected character at byte {start}"),
        };
        Ok(std::mem::replace(&mut self.token, next))
    }

    fn symbol(&mut self, expected: u8) -> Result<()> {
        ensure!(
            matches!(self.token, Token::Symbol(ch) if ch == expected),
            "expected {:?} near byte {}",
            char::from(expected),
            self.position
        );
        self.advance()?;
        Ok(())
    }

    fn expression(&mut self, minimum: u8, depth: usize) -> Result<()> {
        ensure!(
            depth <= self.max_depth,
            "expression exceeds {} nested operations",
            self.max_depth
        );
        match self.advance()? {
            Token::Number(value) => self.program.push(Instruction::Value(value)),
            Token::Name(name) if matches!(self.token, Token::Symbol(b'(')) => {
                let function = Function::named(&name)?;
                self.symbol(b'(')?;
                let mut count = 0;
                if !matches!(self.token, Token::Symbol(b')')) {
                    loop {
                        self.expression(0, depth + 1)?;
                        count += 1;
                        if !matches!(self.token, Token::Symbol(b',')) {
                            break;
                        }
                        self.advance()?;
                    }
                }
                self.symbol(b')')?;
                ensure!(
                    function.accepts(count),
                    "wrong number of arguments ({count}) for {name}"
                );
                self.program.push(Instruction::Call(function, count));
            }
            Token::Name(name) => self.program.push(match name.as_str() {
                "x" => Instruction::X,
                "pi" => Instruction::Value(std::f64::consts::PI),
                "e" => Instruction::Value(std::f64::consts::E),
                _ => bail!("unknown variable or constant {name:?}; use x, pi, or e"),
            }),
            Token::Symbol(b'+') => self.expression(30, depth + 1)?,
            Token::Symbol(b'-') => {
                self.expression(30, depth + 1)?;
                self.program
                    .push(Instruction::Call(Function::Unary(|x| -x), 1));
            }
            Token::Symbol(b'(') => {
                self.expression(0, depth + 1)?;
                self.symbol(b')')?;
            }
            _ => bail!("expected expression near byte {}", self.position),
        }
        loop {
            let (left, right, function): (u8, u8, fn(f64, f64) -> f64) = match self.token {
                Token::Symbol(b'+') => (10, 11, |a, b| a + b),
                Token::Symbol(b'-') => (10, 11, |a, b| a - b),
                Token::Symbol(b'*') => (20, 21, |a, b| a * b),
                Token::Symbol(b'/') => (20, 21, |a, b| a / b),
                Token::Symbol(b'%') => (20, 21, |a, b| a % b),
                Token::Symbol(b'^') => (40, 40, f64::powf),
                _ => break,
            };
            if left < minimum {
                break;
            }
            self.advance()?;
            self.expression(right, depth + 1)?;
            self.program
                .push(Instruction::Call(Function::Binary(function), 2));
        }
        Ok(())
    }
}

#[derive(Debug, Serialize)]
pub struct FunctionReport {
    pub expression: Option<String>,
    pub policy: FunctionPolicy,
    pub observed_min: f64,
    pub observed_max: f64,
    pub outside_unit_range_percent: f64,
}

pub fn apply(
    values: &mut [f64],
    hist: &Histogram,
    expression: Option<&Expression>,
    policy: FunctionPolicy,
) -> Result<FunctionReport> {
    ensure!(values.len() == LEVELS, "invalid expression lookup table");
    let mut function = expression.map(Expression::bind);
    let mut min = f64::INFINITY;
    let mut max = f64::NEG_INFINITY;
    let mut outside = 0u64;
    for (code, &count) in hist.bins.iter().enumerate().filter(|(_, n)| **n > 0) {
        let x = values[code];
        let y = function.as_mut().map_or(x, |f| f(x));
        ensure!(
            y.is_finite(),
            "function is undefined/non-finite at x={x:.17} (raw code {code}); clip/scale/wrap cannot repair NaN or infinity"
        );
        min = min.min(y);
        max = max.max(y);
        if !(0.0..=1.0).contains(&y) {
            outside += count;
        }
        values[code] = y;
    }
    if policy == FunctionPolicy::Scale {
        ensure!(
            max > min,
            "cannot scale a constant function on the pixels in this image"
        );
    }
    for (code, _) in hist.bins.iter().enumerate().filter(|(_, n)| **n > 0) {
        values[code] = match policy {
            FunctionPolicy::Clip => values[code].clamp(0.0, 1.0),
            FunctionPolicy::Scale => {
                // Scale before subtracting if the finite endpoints' difference overflows.
                let range = max - min;
                if range.is_finite() {
                    ((values[code] - min) / range).clamp(0.0, 1.0)
                } else {
                    ((values[code] * 0.5 - min * 0.5) / (max * 0.5 - min * 0.5)).clamp(0.0, 1.0)
                }
            }
            FunctionPolicy::Wrap => {
                if (0.0..=1.0).contains(&values[code]) {
                    values[code]
                } else {
                    values[code].rem_euclid(1.0)
                }
            }
        };
    }
    Ok(FunctionReport {
        expression: expression.map(|expr| expr.source.clone()),
        policy,
        observed_min: min,
        observed_max: max,
        outside_unit_range_percent: 100.0 * outside as f64 / hist.total as f64,
    })
}
