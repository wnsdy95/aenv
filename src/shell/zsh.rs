use super::shims_path_str;

pub fn script() -> String {
    let shims = match shims_path_str() {
        Ok(s) => s,
        Err(msg) => return format!("echo {msg:?} >&2\nreturn 1\n"),
    };
    format!(
        r#"# aenv shell init (zsh)
# Force the shims dir to the FRONT of PATH every time this script is
# eval'd. Idempotent: strip any prior copy first (so a later prepend
# wins) — guards against the case where another tool (asdf/mise/nvm,
# manual `export PATH=...`) added shims further down the PATH or
# re-ordered PATH after `aenv shell-init` ran. Without this, a real
# claude binary higher in PATH would shadow the shim and bypass aenv
# entirely. `typeset -U path` keeps `path` deduplicated; assigning to
# `path` keeps `PATH` in sync (zsh array linkage).
typeset -U path PATH
path=({shims} ${{path:#{shims}}})
export PATH

# Refresh AENV_PROMPT on every prompt based on cwd-resolved env.
# `global` is the universal escape-hatch alias for the user's real
# ~/.claude — when it's active we render an empty prompt tag so the
# shell looks exactly like a non-aenv shell (mirrors Python venv's
# `deactivate` UX). Other envs render `[name] `.
_aenv_prompt() {{
  local _name
  _name=$(AENV_QUIET=1 command aenv current 2>/dev/null)
  if [[ -z "$_name" || "$_name" == "(none)" || "$_name" == "global" ]]; then
    AENV_PROMPT=""
  else
    AENV_PROMPT="[$_name] "
  fi
}}
autoload -Uz add-zsh-hook 2>/dev/null
if (( $+functions[add-zsh-hook] )); then
  add-zsh-hook precmd _aenv_prompt
else
  precmd_functions=(${{precmd_functions[@]}} _aenv_prompt)
fi
_aenv_prompt
if [[ "${{PROMPT-}}" != *AENV_PROMPT* ]]; then
  PROMPT='${{AENV_PROMPT}}'"${{PROMPT-}}"
fi

aenv() {{
  case "$1" in
    # `aenv use <name>` does two things: writes the `.aenv-version`
    # pin (persistent for cwd) AND activates the env in this shell
    # immediately. We export $AENV (resolve step 2) AND drop any
    # AENV_OVERRIDE a prior `aenv quit` may have set (resolve step 1
    # would otherwise pin global and shadow the use). Order matters —
    # binary runs first (validates / can fail), then exports, so a
    # binary error never leaves shell vars in a half-set state.
    use)
      command aenv "$@" || return $?
      # Pull the first positional after `use` (skipping flags) as
      # the env name. Mirrors clap's positional-after-flags parsing
      # so `aenv use foo --global` and `aenv use --global foo` both
      # find "foo".
      local _name=""
      shift
      for _arg in "$@"; do
        case "$_arg" in
          --*|-*) ;;
          *) _name="$_arg"; break ;;
        esac
      done
      if [[ -n "$_name" ]]; then
        unset AENV_OVERRIDE
        export AENV="$_name"
      fi
      _aenv_prompt
      ;;
    # `quit` / `deactivate` mirror Python venv: this shell becomes
    # env-less *visually and behaviorally*. We drop $AENV (any prior
    # `aenv use`) AND set AENV_OVERRIDE=global. The override sits at
    # resolve precedence step 1, so any cwd `.aenv-version` /
    # `aenv.toml` is shadowed for this shell only — the disk pin is
    # NOT touched, so a new shell or `aenv use <name>` re-engages it.
    # `_aenv_prompt` then sees `current=global` and clears the prompt
    # tag, matching what users expect from `deactivate`.
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
