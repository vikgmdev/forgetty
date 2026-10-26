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

_forgetty_precmd() {
  local ret=$?
  if [[ -n $_forgetty_cmd_ran ]]; then
    printf '\e]133;D;%d\a' "$ret"
    _forgetty_cmd_ran=
  fi
  _forgetty_osc7
  # A and B go into the prompt, so a redraw of the prompt marks itself. A
  # prompt theme that rebuilds PS1 gets the marks again here.
  if [[ $PS1 != *$'\e]133;A\a'* ]]; then
    PS1=$'%{\e]133;A\a%}'"$PS1"$'%{\e]133;B\a%}'
  fi
  return $ret
}

_forgetty_preexec() {
  _forgetty_cmd_ran=1
  printf '\e]133;C\a'
}

autoload -Uz add-zsh-hook
add-zsh-hook preexec _forgetty_preexec
add-zsh-hook precmd _forgetty_precmd
# First in the list, so $? is still the status of the command.
precmd_functions=(_forgetty_precmd ${precmd_functions:#_forgetty_precmd})
# This file is loaded from inside the first precmd: mark that prompt too.
_forgetty_precmd
