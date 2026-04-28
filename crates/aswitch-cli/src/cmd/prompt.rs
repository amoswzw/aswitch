use std::io::{self, BufRead, IsTerminal, Write};

use anyhow::{bail, Context, Result};

#[derive(Clone, Debug)]
pub(crate) struct InteractiveOption<T> {
    pub(crate) label: String,
    pub(crate) value: T,
    pub(crate) keys: Vec<String>,
}

pub(crate) fn ensure_interactive_terminal(action: &str) -> Result<()> {
    if !io::stdin().is_terminal() || (!io::stdout().is_terminal() && !io::stderr().is_terminal()) {
        bail!(
            "{action} requires an interactive terminal; provide explicit arguments and try again"
        );
    }

    Ok(())
}

pub(crate) fn prompt_option<R, W, T>(
    input: &mut R,
    output: &mut W,
    title: &str,
    prompt: &str,
    options: &[InteractiveOption<T>],
) -> Result<Option<T>>
where
    R: BufRead,
    W: Write,
    T: Clone,
{
    writeln!(output, "{title}")?;
    for (index, option) in options.iter().enumerate() {
        writeln!(output, "  {}. {}", index + 1, option.label)?;
    }

    loop {
        write!(output, "{prompt} [1-{} or q]: ", options.len())?;
        output.flush()?;

        let Some(line) = read_prompt_line(input)? else {
            return Ok(None);
        };

        if line.is_empty() {
            continue;
        }
        if is_cancel_input(&line) {
            return Ok(None);
        }
        if let Ok(index) = line.parse::<usize>() {
            if index >= 1 && index <= options.len() {
                return Ok(Some(options[index - 1].value.clone()));
            }
        }

        if let Some(option) = options.iter().find(|option| {
            option
                .keys
                .iter()
                .any(|key| key.eq_ignore_ascii_case(line.as_str()))
        }) {
            return Ok(Some(option.value.clone()));
        }

        writeln!(output, "Invalid input. Try again.")?;
    }
}

pub(crate) fn prompt_confirmation<R, W>(input: &mut R, output: &mut W, prompt: &str) -> Result<bool>
where
    R: BufRead,
    W: Write,
{
    loop {
        write!(output, "{prompt}? [y/N]: ")?;
        output.flush()?;

        let Some(line) = read_prompt_line(input)? else {
            return Ok(false);
        };

        if line.is_empty() || matches_ignore_ascii_case(&line, &["n", "no"]) {
            return Ok(false);
        }
        if matches_ignore_ascii_case(&line, &["y", "yes"]) {
            return Ok(true);
        }

        writeln!(output, "Please enter y or n.")?;
    }
}

pub(crate) fn prompt_required_value<R, W>(
    input: &mut R,
    output: &mut W,
    prompt: &str,
) -> Result<Option<String>>
where
    R: BufRead,
    W: Write,
{
    loop {
        write!(output, "{prompt} [enter q to cancel]: ")?;
        output.flush()?;

        let Some(line) = read_prompt_line(input)? else {
            return Ok(None);
        };

        if line.is_empty() {
            writeln!(output, "Input cannot be empty. Try again.")?;
            continue;
        }
        if is_cancel_input(&line) {
            return Ok(None);
        }

        return Ok(Some(line));
    }
}

pub(crate) fn prompt_required_value_in_terminal(prompt: &str) -> Result<Option<String>> {
    ensure_interactive_terminal("This action")?;

    let stdin = io::stdin();
    let stdout = io::stderr();
    let mut input = stdin.lock();
    let mut output = stdout.lock();
    prompt_required_value(&mut input, &mut output, prompt)
}

fn read_prompt_line<R>(input: &mut R) -> Result<Option<String>>
where
    R: BufRead,
{
    let mut line = String::new();
    let bytes = input
        .read_line(&mut line)
        .context("failed to read prompt input")?;
    if bytes == 0 {
        return Ok(None);
    }
    Ok(Some(line.trim().to_string()))
}

fn is_cancel_input(value: &str) -> bool {
    matches_ignore_ascii_case(value, &["q", "quit", "exit"])
}

fn matches_ignore_ascii_case(value: &str, candidates: &[&str]) -> bool {
    candidates
        .iter()
        .any(|candidate| value.eq_ignore_ascii_case(candidate))
}
