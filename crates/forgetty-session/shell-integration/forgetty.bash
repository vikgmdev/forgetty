# Forgetty shell integration for bash.
#
# Writes the standard prompt marks (OSC 133 A, B, C, D with the exit code) and
# the working directory (OSC 7) so the Forgetty server can tell a prompt from a
# running command. The server starts this from PROMPT_COMMAND for shells it
# starts, without touching your rc files. You can also source this file by hand.
[[ $- == *i* ]] || return 0
[[ -n ${__forgetty_si_loaded-} ]] && return 0
__forgetty_si_loaded=1
__forgetty_ran=

# The server's one-shot start line leaves PROMPT_COMMAND here.
__forgetty_boot='. "$FORGETTY_SHELL_INTEGRATION_DIR/forgetty.bash"'
if [[ ${PROMPT_COMMAND-} == *"$__forgetty_boot"* ]]; then
  PROMPT_COMMAND=${PROMPT_COMMAND/"$__forgetty_boot"/}
  PROMPT_COMMAND=${PROMPT_COMMAND#"${PROMPT_COMMAND%%[![:space:];]*}"}
  [[ -z ${PROMPT_COMMAND//[[:space:];]/} ]] && unset PROMPT_COMMAND
fi
unset __forgetty_boot
export -n PROMPT_COMMAND FORGETTY_SHELL_INTEGRATION_DIR

__forgetty_osc7() {
  local LC_ALL=C i c out= hex
  for (( i = 0; i < ${#PWD}; i++ )); do
    c=${PWD:i:1}
    case $c in
      [a-zA-Z0-9/._~-]) out+=$c ;;
      *) printf -v hex '%%%02X' "'$c"; out+=$hex ;;
    esac
  done
  printf '\e]7;file://%s%s\a' "${HOSTNAME}" "$out"
}

# First in PROMPT_COMMAND, so $? is still the status of the command.
__forgetty_first() {
  local ret=$?
  if [[ -n $__forgetty_ran ]]; then
    printf '\e]133;D;%s\a' "$ret"
    __forgetty_ran=
  fi
  __forgetty_osc7
  return $ret
}

# Last in PROMPT_COMMAND, after a prompt theme (starship, powerline, ...) has
# rebuilt PS1. A and B go into PS1, so a redraw of the prompt marks itself.
__forgetty_last() {
  local ret=$?
  if [[ $PS1 != *'133;A'* ]]; then
    PS1='\[\e]133;A\a\]'"$PS1"'\[\e]133;B\a\]'
  fi
  return $ret
}

# C is written by the shell itself, after the line is read and before the
# command runs. The expansion also records that a command ran.
PS0='\e]133;C;${__forgetty_ran:=1}\a'"${PS0-}"

if [[ $(declare -p PROMPT_COMMAND 2>/dev/null) == "declare -a"* ]]; then
  PROMPT_COMMAND=(__forgetty_first "${PROMPT_COMMAND[@]}" __forgetty_last)
else
  # A newline before the last hook keeps a trailing comment or & in the user's
  # command from swallowing it.
  PROMPT_COMMAND="__forgetty_first;${PROMPT_COMMAND:+$PROMPT_COMMAND$'\n'}__forgetty_last"
fi

# Programs started from this shell (a nested bash, su, script, vim :sh, tmux)
# must not inherit the hooks: the functions exist only in this shell.
export -n PROMPT_COMMAND FORGETTY_SHELL_INTEGRATION_DIR

# This file runs from inside the first PROMPT_COMMAND: mark that prompt too.
__forgetty_first
__forgetty_last
