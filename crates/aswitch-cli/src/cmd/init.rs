use std::env;

use anyhow::{bail, Context, Result};
use clap::Args;

use super::session;

#[derive(Debug, Args)]
#[command(about = "Print shell integration for shell and project scopes")]
pub struct InitArgs {
    pub shell: Option<String>,
}

pub fn run(args: InitArgs) -> Result<()> {
    let shell = detect_shell(args.shell)?;
    let shell = match shell.as_str() {
        "zsh" | "bash" => shell,
        other => bail!("unsupported shell: {other}; expected zsh or bash"),
    };

    let binary = env::current_exe().context("failed to resolve current executable")?;
    let binary = session::shell_quote(binary.to_string_lossy().as_ref());
    let shell_integration = render_common_script(&binary);

    let script = match shell.as_str() {
        "zsh" => format!("{shell_integration}\n{}", render_zsh_hook()),
        "bash" => format!("{shell_integration}\n{}", render_bash_hook()),
        _ => unreachable!(),
    };

    println!("{script}");
    Ok(())
}

fn detect_shell(explicit: Option<String>) -> Result<String> {
    if let Some(shell) = explicit {
        return Ok(shell);
    }

    let shell = env::var("SHELL")
        .context("failed to detect shell; pass `aswitch init zsh` or `aswitch init bash`")?;
    let detected = shell
        .rsplit('/')
        .next()
        .filter(|value| !value.is_empty())
        .context("failed to detect shell; pass `aswitch init zsh` or `aswitch init bash`")?;
    Ok(detected.to_string())
}

fn render_common_script(binary: &str) -> String {
    format!(
        r#"export ASWITCH_BIN={binary}

_aswitch_bin() {{
  "$ASWITCH_BIN" "$@"
}}

_aswitch_has_flag() {{
  local needle="$1"
  shift
  for arg in "$@"; do
    if [ "$arg" = "$needle" ]; then
      return 0
    fi
  done
  return 1
}}

_aswitch_flag_value() {{
  local needle="$1"
  shift
  local take_next=0
  for arg in "$@"; do
    if [ "$take_next" -eq 1 ]; then
      printf '%s\n' "$arg"
      return 0
    fi
    case "$arg" in
      "$needle")
        take_next=1
        ;;
      "$needle"=*)
        printf '%s\n' "${{arg#*=}}"
        return 0
        ;;
    esac
  done
  return 1
}}

_aswitch_scope() {{
  local scope
  scope="$(_aswitch_flag_value --scope "$@")" || true
  if [ -n "$scope" ]; then
    printf '%s\n' "$scope"
  else
    printf 'shell\n'
  fi
}}

_aswitch_sync_project() {{
  local output
  output="$("$ASWITCH_BIN" __shell sync-project)" || return $?
  if [ -n "$output" ]; then
    eval "$output"
  fi
}}

aswitch() {{
  if [ "$#" -eq 0 ]; then
    "$ASWITCH_BIN"
    return $?
  fi

    case "$1" in
    use)
      shift
      local scope
      scope="$(_aswitch_scope "$@")"
      case "$scope" in
      global)
        "$ASWITCH_BIN" use "$@"
        return $?
        ;;
      project)
        "$ASWITCH_BIN" use "$@"
        local status=$?
        if [ $status -eq 0 ]; then
          _aswitch_sync_project || return $?
        fi
        return $status
        ;;
      shell|"")
        local output
        output="$("$ASWITCH_BIN" __shell use "$@")" || return $?
        if [ -n "$output" ]; then
          eval "$output"
        fi
        if _aswitch_has_flag --off "$@"; then
          _aswitch_sync_project || return $?
        fi
        return 0
        ;;
      *)
        "$ASWITCH_BIN" use "$@"
        return $?
        ;;
      esac
      ;;
    *)
      "$ASWITCH_BIN" "$@"
      return $?
      ;;
  esac
}}

"#
    )
}

fn render_zsh_hook() -> &'static str {
    r#"typeset -ga chpwd_functions
if [[ -z "${chpwd_functions[(r)_aswitch_sync_project]}" ]]; then
  chpwd_functions+=(_aswitch_sync_project)
fi
_aswitch_sync_project
"#
}

fn render_bash_hook() -> &'static str {
    r#"case ";${PROMPT_COMMAND:-};" in
  *";_aswitch_sync_project;"*) ;;
  *) PROMPT_COMMAND="_aswitch_sync_project${PROMPT_COMMAND:+;$PROMPT_COMMAND}" ;;
esac
_aswitch_sync_project
"#
}
