# Forgetty shell integration for zsh.
#
# Writes the standard prompt marks (OSC 133 A, B, C, D with the exit code) and
# the working directory (OSC 7) so the Forgetty server can tell a prompt from a
# running command. The server sets this up for shells it starts, without
# touching your rc files. You can also source this file by hand.
[[ -o interactive ]] || return 0
[[ -n ${_forgetty_si_loaded-} ]] && return 0
typeset -g _forgetty_si_loaded=1
typeset -g _forgetty_cmd_ran=

_forgetty_osc7() {
  local LC_ALL=C c out= hex i
  for (( i = 1; i <= ${#PWD}; i++ )); do
    c=${PWD[i]}
    case $c in
      [a-zA-Z0-9/._~-]) out+=$c ;;
      *) printf -v hex '%%%02X' "'$c"; out+=$hex ;;
    esac
  done
  printf '\e]7;file://%s%s\a' "${HOST}" "$out"
}

# First in precmd_functions, so $? is still the status of the command.
_forgetty_precmd() {
  local ret=$?
  if [[ -n $_forgetty_cmd_ran ]]; then
    printf '\e]133;D;%d\a' "$ret"
    _forgetty_cmd_ran=
  fi
  _forgetty_osc7
  return $ret
}

# Last in precmd_functions, after a prompt theme (powerlevel10k, starship, ...)
# has rebuilt PS1. A and B go into the prompt, so a redraw of the prompt marks
# itself.
_forgetty_precmd_last() {
  local ret=$?
  if [[ $PS1 != *$'\e]133;A\a'* ]]; then
    PS1=$'%{\e]133;A\a%}'"$PS1"$'%{\e]133;B\a%}'
  fi
  return $ret
}

# A theme or plugin that adds its precmd later goes behind the hook above:
# put it back at the end before every command.
_forgetty_preexec() {
  _forgetty_cmd_ran=1
  printf '\e]133;C\a'
  precmd_functions=(${precmd_functions:#_forgetty_precmd_last} _forgetty_precmd_last)
}

autoload -Uz add-zsh-hook
add-zsh-hook preexec _forgetty_preexec
add-zsh-hook precmd _forgetty_precmd
add-zsh-hook precmd _forgetty_precmd_last
precmd_functions=(_forgetty_precmd ${precmd_functions:#(_forgetty_precmd|_forgetty_precmd_last)} _forgetty_precmd_last)

# Programs started from this shell must not inherit the integration path.
typeset +x FORGETTY_SHELL_INTEGRATION_DIR

# This file is loaded from inside the first precmd: mark that prompt too.
_forgetty_precmd
_forgetty_precmd_last
