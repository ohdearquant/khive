use std::io::Write;

use anyhow::Result;

use super::{
    record_prefix, FixSummary, OutputFormat, ValidateArgs, ValidationReport, ValidationRun,
};

pub(super) fn render_validation(
    run: &ValidationRun,
    args: &ValidateArgs,
    stdout: &mut dyn Write,
    stderr: &mut dyn Write,
) -> Result<()> {
    match args.format {
        OutputFormat::Json => {
            let json =
                serde_json::to_string_pretty(&run.report).expect("serialize ValidationReport");
            writeln!(stdout, "{json}")?;
        }
        OutputFormat::Github => write_github_format(stdout, &run.report)?,
        OutputFormat::Text => write_text_format(stdout, run, args.verbose, args.quiet)?,
    }
    if !matches!(args.format, OutputFormat::Text) {
        if let Some(fixes) = &run.fixes {
            if !args.quiet {
                write_fix_details(stderr, fixes)?;
            }
            write_summary(stderr, &run.report, Some(fixes))?;
        }
    }
    Ok(())
}

fn write_fix_details(output: &mut dyn Write, fixes: &FixSummary) -> std::io::Result<()> {
    for fix in &fixes.files {
        writeln!(
            output,
            "  ~ sort-order: re-sorted {} ({} lines moved)",
            fix.file, fix.moved
        )?;
    }
    Ok(())
}

fn write_text_format(
    output: &mut dyn Write,
    run: &ValidationRun,
    verbose: bool,
    quiet: bool,
) -> std::io::Result<()> {
    let report = &run.report;
    if !quiet {
        let mut structural_sort_seen = false;
        for r in &report.rules {
            if r.id == "sort-order" && !structural_sort_seen {
                structural_sort_seen = true;
                if let Some(fixes) = &run.fixes {
                    if !fixes.files.is_empty() {
                        write_fix_details(output, fixes)?;
                        if r.passed {
                            continue;
                        }
                    }
                }
            }
            let symbol = if r.passed {
                "\u{2713}"
            } else if r.severity == "error" {
                "\u{2717}"
            } else {
                "\u{26a0}"
            };
            if r.violations.is_empty() {
                writeln!(output, "  {symbol} {}", r.id)?;
            } else {
                writeln!(
                    output,
                    "  {symbol} {}: {} violation(s)",
                    r.id,
                    r.violations.len()
                )?;
                let shown = if verbose {
                    r.violations.len()
                } else {
                    2.min(r.violations.len())
                };
                for v in &r.violations[..shown] {
                    let prefix = record_prefix(v.entity_id.as_deref(), v.entity_name.as_deref());
                    // Include record identifier when not already in the message.
                    if prefix.is_empty() || v.message.starts_with(prefix.trim()) {
                        writeln!(output, "    - {}", v.message)?;
                    } else {
                        writeln!(output, "    - {}{}", prefix, v.message)?;
                    }
                }
                if !verbose && r.violations.len() > 2 {
                    writeln!(
                        output,
                        "    + {} more (run with --verbose)",
                        r.violations.len() - 2
                    )?;
                }
            }
        }
    }
    write_summary(output, report, run.fixes.as_ref())
}

fn write_summary(
    output: &mut dyn Write,
    report: &ValidationReport,
    fixes: Option<&FixSummary>,
) -> std::io::Result<()> {
    let s = &report.summary;
    let empty = if s.empty { "; empty graph" } else { "" };
    if let Some(fixes) = fixes {
        writeln!(
            output,
            "\nSummary: {} error(s) fixed, {} error(s) unfixable, {} warning(s) fixed, {} warning(s) unfixable, {} info(s) fixed, {} info(s) unfixable, {} entities, {} edges{empty}",
            fixes.fixed.errors, s.errors, fixes.fixed.warnings, s.warnings,
            fixes.fixed.info, s.info, s.entities, s.edges
        )
    } else {
        writeln!(
            output,
            "\nSummary: {} error(s), {} warning(s), {} entities, {} edges{empty}",
            s.errors, s.warnings, s.entities, s.edges
        )
    }
}

fn write_github_format(output: &mut dyn Write, report: &ValidationReport) -> std::io::Result<()> {
    if report.summary.empty {
        writeln!(
            output,
            "::notice ::Empty graph: no entity, edge, or note records read"
        )?;
    }
    for r in &report.rules {
        for v in &r.violations {
            let level = if r.severity == "error" {
                "error"
            } else {
                "warning"
            };
            writeln!(output, "::{level} ::{}", v.message)?;
        }
    }
    Ok(())
}
