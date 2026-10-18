//! Arguments for the custom p95 driver, validated before any workload starts.

#[derive(Debug, PartialEq, Eq)]
pub(super) enum Mode {
    List,
    Smoke,
    ReportContract,
    Regression(String),
    Measure,
}

pub(super) fn refuse(message: &str) -> ! {
    eprintln!(
        "{message}\nAccepted: --list, --test, --bench, --regression-case=name, \
         --quick, --noplot, --save-baseline NAME"
    );
    std::process::exit(2);
}

pub(super) fn parse(args: &[String]) -> Result<Mode, String> {
    let mut list = false;
    let mut test = false;
    let mut bench = false;
    let mut regression = None;
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--list" => list = true,
            "--test" => test = true,
            "--bench" => bench = true,
            "--noplot" | "--quick" => {}
            "--save-baseline" => {
                let value = args.next().ok_or("--save-baseline requires NAME")?;
                validate_baseline(value)?;
            }
            "--regression-case" => {
                return Err("use --regression-case=name, not a separate value".into());
            }
            _ => {
                if let Some(value) = arg.strip_prefix("--regression-case=") {
                    if regression.replace(value.to_owned()).is_some() {
                        return Err("only one --regression-case=name is accepted".into());
                    }
                    if value != "report-contract"
                        && value != "list-only"
                        && !super::regression::CASES
                            .iter()
                            .any(|(name, _)| *name == value)
                    {
                        return Err(format!("unknown regression case: {value}"));
                    }
                } else if let Some(value) = arg.strip_prefix("--save-baseline=") {
                    validate_baseline(value)?;
                } else {
                    return Err(format!("unrecognised argument: {arg}"));
                }
            }
        }
    }

    // Validate all arguments before taking this fixture-free early return.
    if list {
        return Ok(Mode::List);
    }
    if test {
        if regression.is_some() {
            return Err("--test cannot be combined with --regression-case=name".into());
        }
        return Ok(Mode::Smoke);
    }
    match regression {
        Some(case) if case == "report-contract" => Ok(Mode::ReportContract),
        Some(case) => Ok(Mode::Regression(case)),
        None if bench => Ok(Mode::Measure),
        None => Ok(Mode::Smoke),
    }
}

fn validate_baseline(value: &str) -> Result<(), String> {
    if value.is_empty() || value.starts_with('-') {
        Err("--save-baseline requires a nonempty NAME, not another option".into())
    } else {
        Ok(())
    }
}
