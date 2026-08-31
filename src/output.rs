// SPDX-License-Identifier: AGPL-3.0-or-later
//! Consistent command-result rendering. Operational logs belong on stderr.

use clap::ValueEnum;
use serde::Serialize;
use std::io::{self, IsTerminal as _, Write};

pub const OUTPUT_SCHEMA_VERSION: u16 = 1;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, ValueEnum)]
pub enum OutputFormat {
    #[default]
    Auto,
    Pretty,
    Text,
    Json,
}

impl OutputFormat {
    pub fn resolved(self) -> Self {
        match self {
            Self::Auto if io::stdout().is_terminal() => Self::Pretty,
            Self::Auto => Self::Text,
            explicit => explicit,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Warning {
    pub code: String,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
}

pub struct CommandOutput<T> {
    pub data: T,
    pub pretty: String,
    pub text: String,
    pub warnings: Vec<Warning>,
}

impl<T> CommandOutput<T> {
    pub fn new(data: T, pretty: impl Into<String>, text: impl Into<String>) -> Self {
        Self {
            data,
            pretty: pretty.into(),
            text: text.into(),
            warnings: Vec::new(),
        }
    }

    pub fn warnings(mut self, warnings: Vec<Warning>) -> Self {
        self.warnings = warnings;
        self
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Envelope<'a, T> {
    schema_version: u16,
    data: &'a T,
    warnings: &'a [Warning],
}

pub fn emit<T: Serialize>(format: OutputFormat, output: &CommandOutput<T>) -> io::Result<()> {
    let mut stdout = io::stdout().lock();
    match format.resolved() {
        OutputFormat::Pretty => line(&mut stdout, &output.pretty)?,
        OutputFormat::Text => line(&mut stdout, &output.text)?,
        OutputFormat::Json => {
            serde_json::to_writer_pretty(
                &mut stdout,
                &Envelope {
                    schema_version: OUTPUT_SCHEMA_VERSION,
                    data: &output.data,
                    warnings: &output.warnings,
                },
            )?;
            stdout.write_all(b"\n")?;
        }
        OutputFormat::Auto => unreachable!("auto output is resolved before rendering"),
    }
    if format.resolved() != OutputFormat::Json {
        for warning in &output.warnings {
            let source = warning
                .source
                .as_deref()
                .map_or_else(String::new, |source| format!(" ({source})"));
            eprintln!("warning [{}]{source}: {}", warning.code, warning.message);
        }
    }
    Ok(())
}

fn line(output: &mut impl Write, value: &str) -> io::Result<()> {
    output.write_all(value.as_bytes())?;
    if !value.ends_with('\n') {
        output.write_all(b"\n")?;
    }
    Ok(())
}

pub fn table(headers: &[&str], rows: &[Vec<String>]) -> String {
    if rows.is_empty() {
        return "No results.".into();
    }
    let mut widths = headers
        .iter()
        .map(|header| header.len())
        .collect::<Vec<_>>();
    for row in rows {
        for (index, value) in row.iter().enumerate().take(widths.len()) {
            widths[index] = widths[index].max(value.chars().count());
        }
    }
    let render = |values: Vec<String>| {
        values
            .into_iter()
            .enumerate()
            .map(|(index, value)| {
                if index + 1 == widths.len() {
                    value
                } else {
                    format!("{value:<width$}", width = widths[index])
                }
            })
            .collect::<Vec<_>>()
            .join("  ")
    };
    let mut lines = vec![render(
        headers.iter().map(|value| (*value).into()).collect(),
    )];
    lines.extend(rows.iter().cloned().map(render));
    lines.join("\n")
}

pub fn text_rows(rows: &[Vec<String>]) -> String {
    rows.iter()
        .map(|row| row.join("\t"))
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn table_aligns_columns_without_decorative_borders() {
        assert_eq!(
            table(
                &["NAME", "STATUS"],
                &[
                    vec!["desk".into(), "ready".into()],
                    vec!["warehouse".into(), "offline".into()],
                ],
            ),
            "NAME       STATUS\ndesk       ready\nwarehouse  offline"
        );
    }

    #[test]
    fn text_rows_are_line_oriented_and_stable() {
        assert_eq!(
            text_rows(&[vec!["desk".into(), "ready".into()]]),
            "desk\tready"
        );
    }
}
