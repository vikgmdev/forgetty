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

# bash 5.1 and newer run every element of an array PROMPT_COMMAND; older bash
# uses element 0 only, so there the string form is used. (The version can be
# forced for a test with FORGETTY_BASH_VERSION_FOR_TEST.)
__forgetty_arr=
__forgetty_v=${FORGETTY_BASH_VERSION_FOR_TEST:-${BASH_VERSINFO[0]}.${BASH_VERSINFO[1]}}
case $__forgetty_v in
  [0-4].* | 5.0 | 5.0.*) ;;
  *) __forgetty_arr=1 ;;
esac
unset __forgetty_v

# The server's one-shot start line leaves PROMPT_COMMAND here.
__forgetty_boot='. "$FORGETTY_SHELL_INTEGRATION_DIR/forgetty.bash"'
if [[ -n $__forgetty_arr && ${PROMPT_COMMAND@a} == *a* ]]; then
  # A startup file made it an array (`PROMPT_COMMAND+=(hook)`): the start line
  # is one element. Take it out and keep the user's other elements.
  for __forgetty_i in "${!PROMPT_COMMAND[@]}"; do
    __forgetty_e=${PROMPT_COMMAND[__forgetty_i]}
    if [[ $__forgetty_e == *"$__forgetty_boot"* ]]; then
      __forgetty_e=${__forgetty_e/"$__forgetty_boot"/}
      __forgetty_e=${__forgetty_e#"${__forgetty_e%%[![:space:];]*}"}
      if [[ -z ${__forgetty_e//[[:space:];]/} ]]; then
        unset "PROMPT_COMMAND[$__forgetty_i]"
      else
        PROMPT_COMMAND[__forgetty_i]=$__forgetty_e
      fi
    fi
  done
  unset __forgetty_i __forgetty_e
elif [[ ${PROMPT_COMMAND-} == *"$__forgetty_boot"* ]]; then
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

# Put the list in order again: __forgetty_first at the front, __forgetty_last at
# the end. A hook added after the start (`PROMPT_COMMAND+=(x)`, `+=";x"`) lands
# behind the last hook, and one added in front (`PROMPT_COMMAND=(x ...)`) pushes
# the first hook back. This takes effect from the next prompt: the prompt that
# is running now has its list already. No external command and no subshell runs
# here: it is on the path of every prompt.
# Sets __forgetty_displaced when the first hook was not first in the string
# form: $? is then the status of the hook in front, not of the user's command.
# (bash restores $? before each element of an array, so there it is never set.)
__forgetty_relast() {
  __forgetty_displaced=
  local nl=$'\n' e i
  if [[ -n $__forgetty_arr && ${PROMPT_COMMAND@a} == *a* ]]; then
    local idx=("${!PROMPT_COMMAND[@]}") cur=() out=() want k
    for i in "${idx[@]}"; do
      cur+=("${PROMPT_COMMAND[i]}")
    done
    for e in "${cur[@]}"; do
      [[ $e == __forgetty_first || $e == __forgetty_last ]] && continue
      # the string form of an earlier start, inside an element
      e=${e#"__forgetty_first;"}
      [[ $e == *"${nl}__forgetty_last" ]] && e=${e%"${nl}__forgetty_last"}
      [[ -z ${e//[[:space:];]/} ]] || out+=("$e")
    done
    want=(__forgetty_first "${out[@]}" __forgetty_last)
    if (( ${#want[@]} == ${#cur[@]} )); then
      for (( k = 0; k < ${#want[@]}; k++ )); do
        [[ ${want[k]} == "${cur[k]}" ]] || break
      done
      (( k == ${#want[@]} )) && return 0
    fi
    PROMPT_COMMAND=("${want[@]}")
  else
    local pc=$PROMPT_COMMAND changed=
    if [[ $pc != "__forgetty_first;"* ]]; then
      __forgetty_displaced=1
      pc=${pc/"__forgetty_first;"/}
      pc="__forgetty_first;$pc"
      changed=1
    fi
    if [[ $pc != *"${nl}__forgetty_last" && $pc == *"${nl}__forgetty_last"* ]]; then
      # A no-op in its place keeps the list valid whatever follows it (`;x`).
      pc=${pc/"${nl}__forgetty_last"/"${nl}:"}
      pc="$pc${nl}__forgetty_last"
      changed=1
    fi
    [[ -n $changed ]] && PROMPT_COMMAND=$pc
  fi
  return 0
}

# First in PROMPT_COMMAND, so $? is still the status of the command.
__forgetty_first() {
  local ret=$?
  __forgetty_relast
  if [[ -n $__forgetty_ran ]]; then
    if [[ -n $__forgetty_displaced ]]; then
      # A hook ran in front of this one: the status is not the command's.
      printf '\e]133;D\a'
    else
      printf '\e]133;D;%s\a' "$ret"
    fi
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

if [[ -n $__forgetty_arr && ${PROMPT_COMMAND@a} == *a* ]]; then
  PROMPT_COMMAND=(__forgetty_first "${PROMPT_COMMAND[@]}" __forgetty_last)
else
  # A newline before the last hook keeps a trailing comment or & in the user's
  # command from swallowing it. It is built in a variable: $'\n' inside the
  # double quotes of a ${...:+...} is literal when `extquote` is off.
  __forgetty_nl=$'\n'
  PROMPT_COMMAND="__forgetty_first;${PROMPT_COMMAND-}${__forgetty_nl}__forgetty_last"
  unset __forgetty_nl
fi

# Programs started from this shell (a nested bash, su, script, vim :sh, tmux)
# must not inherit the hooks: the functions exist only in this shell.
export -n PROMPT_COMMAND FORGETTY_SHELL_INTEGRATION_DIR

# This file runs from inside the first PROMPT_COMMAND: mark that prompt too.
__forgetty_first
__forgetty_last
