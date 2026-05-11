use super::shims_path_str;

pub fn script() -> String {
    let shims = match shims_path_str() {
        Ok(s) => s,
        Err(msg) => return format!("echo {msg:?} >&2\nreturn 1\n"),
    };
    format!(
        r#"# aenv shell init (bash)
# Force the shims dir to the FRONT of PATH every time this script is
# eval'd. Strip any prior copy first (so a later prepend wins) —
# guards against another tool (asdf/mise/nvm, manual `export PATH=...`)
# adding shims further down or reordering PATH after `aenv shell-init`
# ran. Without this, a real claude binary higher in PATH would shadow
# the shim and bypass aenv entirely.
_aenv_strip="$(printf %s ":$PATH:" | sed -e 's|:{shims}:|:|g' -e 's|^:||' -e 's|:$||')"
export PATH="{shims}${{_aenv_strip:+:$_aenv_strip}}"
unset _aenv_strip

# Refresh AENV_PROMPT before each prompt and auto-prepend to $PS1.
# `global` is the universal escape-hatch alias for the user's real
# ~/.claude — when active, render an empty tag so the shell looks
# exactly like a non-aenv shell (mirrors Python venv `deactivate`).
_aenv_prompt() {{
  local _name
  _name=$(AENV_QUIET=1 command aenv current 2>/dev/null)
  if [ -z "$_name" ] || [ "$_name" = "(none)" ] || [ "$_name" = "global" ]; then
    AENV_PROMPT=""
  else
    AENV_PROMPT="[$_name] "
  fi
}}
case ";${{PROMPT_COMMAND-}};" in
  *";_aenv_prompt;"*) ;;
  *) PROMPT_COMMAND="_aenv_prompt${{PROMPT_COMMAND:+;$PROMPT_COMMAND}}" ;;
esac
_aenv_prompt
# Git Bash's default PS1 starts with a window-title escape followed by a
# literal `\n`, so naive prepend lands the env tag on its own line above
# the real prompt. We need to splice AENV_PROMPT in *after* the first
# `\n` instead.
#
# Cases handled (order matters — first match wins):
#   1. Already correctly spliced (`\n` then AENV_PROMPT): no-op.
#   2. Wrongly prepended by an older shell-init in the same session
#      (AENV_PROMPT then `\n`): strip the leading prefix and re-splice.
#      Without this, in-place re-eval of a fixed shell-init can't heal
#      the bug — users would have to `exec bash` after every upgrade.
#   3. Already injected on a single-line PS1 (no `\n`): no-op.
#   4. Fresh multi-line PS1: splice after the first `\n`.
#   5. Fresh single-line PS1: prepend at the front.
case "${{PS1-}}" in
  *'\n'*'${{AENV_PROMPT}}'*) ;;
  '${{AENV_PROMPT}}'*'\n'*)
    _aenv_stripped="${{PS1#'${{AENV_PROMPT}}'}}"
    _aenv_before="${{_aenv_stripped%%'\n'*}}"
    _aenv_after="${{_aenv_stripped#*'\n'}}"
    PS1="${{_aenv_before}}"'\n${{AENV_PROMPT}}'"${{_aenv_after}}"
    unset _aenv_stripped _aenv_before _aenv_after
    ;;
  *AENV_PROMPT*) ;;
  *'\n'*)
    _aenv_before="${{PS1%%'\n'*}}"
    _aenv_after="${{PS1#*'\n'}}"
    PS1="${{_aenv_before}}"'\n${{AENV_PROMPT}}'"${{_aenv_after}}"
    unset _aenv_before _aenv_after
    ;;
  *) PS1='${{AENV_PROMPT}}'"${{PS1-}}" ;;
esac

aenv() {{
  case "$1" in
    # `aenv use <name>` writes the cwd pin AND activates the env in
    # this shell. Drops AENV_OVERRIDE so a prior `aenv quit` doesn't
    # shadow the new pin. See the zsh init for full rationale.
    use)
      command aenv "$@" || return $?
      local _name=""
      shift
      for _arg in "$@"; do
        case "$_arg" in
          --*|-*) ;;
          *) _name="$_arg"; break ;;
        esac
      done
      if [ -n "$_name" ]; then
        unset AENV_OVERRIDE
        export AENV="$_name"
      fi
      _aenv_prompt
      ;;
    # `quit` / `deactivate` mirror Python venv: this shell becomes
    # env-less visually and behaviorally. See the zsh init for full
    # rationale; AENV_OVERRIDE=global wins resolve step 1, so the
    # shell looks/behaves like global without touching disk pins.
    quit|deactivate)
      unset AENV
      export AENV_OVERRIDE=global
      _aenv_prompt
      ;;
    *)
      command aenv "$@"
      ;;
  esac
}}
"#
    )
}

#[cfg(test)]
mod tests {
    use super::script;

    #[test]
    fn ps1_splices_after_first_literal_newline() {
        // Lock in the Git Bash fix: when PS1 contains a literal `\n`
        // (e.g., the default Git Bash prompt that has a window-title
        // escape followed by `\n`), AENV_PROMPT must be inserted after
        // that newline, not at the very front. Otherwise the env tag
        // appears on its own (visually empty) line above the prompt.
        let s = script();
        assert!(
            s.contains("*'\\n'*)"),
            "missing case arm for PS1 containing literal \\n: {s}"
        );
        assert!(
            s.contains(r#"_aenv_before="${PS1%%'\n'*}""#),
            "missing before-newline split"
        );
        assert!(
            s.contains(r#"_aenv_after="${PS1#*'\n'}""#),
            "missing after-newline split"
        );
        assert!(
            s.contains(r#"PS1="${_aenv_before}"'\n${AENV_PROMPT}'"${_aenv_after}""#),
            "missing PS1 reassembly"
        );
    }

    #[test]
    fn ps1_fallback_still_prepends_when_no_newline() {
        // Single-line PS1s (most non-Git-Bash setups) keep the old
        // behavior — prepend AENV_PROMPT at the front.
        let s = script();
        assert!(
            s.contains(r#"*) PS1='${AENV_PROMPT}'"${PS1-}" ;;"#),
            "missing default-prepend arm"
        );
    }

    #[test]
    fn ps1_injection_is_idempotent() {
        // Already-injected PS1 must short-circuit so re-eval'ing the
        // shell init doesn't double-prepend.
        let s = script();
        assert!(s.contains("*AENV_PROMPT*) ;;"), "missing idempotence guard");
    }

    #[test]
    fn ps1_repositions_when_old_init_left_aenv_prompt_at_front() {
        // Self-heal path: an older shell-init in the same bash session
        // already prepended `${AENV_PROMPT}` to the front of PS1. The
        // new logic must detect that and move it after the first `\n`,
        // otherwise users would have to `exec bash` after every aenv
        // upgrade for the multi-line fix to take effect.
        let s = script();
        assert!(
            s.contains("'${AENV_PROMPT}'*'\\n'*)"),
            "missing reposition arm"
        );
        assert!(
            s.contains(r#"_aenv_stripped="${PS1#'${AENV_PROMPT}'}""#),
            "missing strip-leading-prefix step"
        );
    }

    #[test]
    fn ps1_already_correctly_spliced_is_noop() {
        // The first arm must match `\n...AENV_PROMPT` (correctly placed
        // already) so a re-eval doesn't re-splice and produce duplicate
        // tags.
        let s = script();
        assert!(
            s.contains("*'\\n'*'${AENV_PROMPT}'*) ;;"),
            "missing already-spliced no-op arm"
        );
    }
}
