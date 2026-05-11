use super::shims_path_str;

pub fn script() -> String {
    let shims = match shims_path_str() {
        Ok(s) => s,
        Err(msg) => return format!("echo {msg:?} >&2\nexit 1\n"),
    };
    format!(
        r#"# aenv shell init (fish)
# Force shims dir to the FRONT of PATH on every eval. Strip any prior
# copy first so a later prepend wins — guards against asdf/mise/nvm or
# a manual `set -gx PATH ...` adding shims further down or reordering
# PATH after `aenv shell-init` ran. Without this, a real claude binary
# higher in PATH would shadow the shim and bypass aenv entirely.
set -l _aenv_idx (contains -i -- "{shims}" $PATH)
if test -n "$_aenv_idx"
  set -e PATH[$_aenv_idx]
end
set -gx PATH "{shims}" $PATH

# Refresh AENV_PROMPT before each prompt; user adds $AENV_PROMPT to
# their fish_prompt function for a `[envname] ` prefix. `global` is
# the universal escape-hatch alias — render an empty tag so the
# shell looks like a non-aenv shell (mirrors Python venv `deactivate`).
function _aenv_prompt --on-event fish_prompt
  set -l _name (AENV_QUIET=1 command aenv current 2>/dev/null)
  if test -z "$_name" -o "$_name" = "(none)" -o "$_name" = "global"
    set -gx AENV_PROMPT ""
  else
    set -gx AENV_PROMPT "[$_name] "
  end
end
_aenv_prompt

function aenv
  switch $argv[1]
    # `aenv use <name>` writes the cwd pin AND activates the env in
    # this shell. Drops AENV_OVERRIDE so a prior `aenv quit` doesn't
    # shadow the new pin. See the zsh init for full rationale.
    case use
      command aenv $argv
      or return $status
      set -l _name ""
      for _arg in $argv[2..-1]
        switch $_arg
          case '--*' '-*'
          case '*'
            set _name $_arg
            break
        end
      end
      if test -n "$_name"
        set -e AENV_OVERRIDE
        set -gx AENV "$_name"
      end
      _aenv_prompt
    # `quit` / `deactivate` mirror Python venv: shell becomes env-less
    # visually and behaviorally. AENV_OVERRIDE=global wins resolve
    # step 1, shadowing any cwd pin for this shell only.
    case quit deactivate
      set -e AENV
      set -gx AENV_OVERRIDE global
      _aenv_prompt
    case '*'
      command aenv $argv
  end
end
"#
    )
}
