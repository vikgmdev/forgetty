# Forgetty: hand the startup files back to the user's own ZDOTDIR, then load
# the integration at the first prompt. The server points ZDOTDIR here for the
# shells it starts. Nothing in your home is edited.
if [[ -n ${FORGETTY_USER_ZDOTDIR+x} ]]; then
  ZDOTDIR=$FORGETTY_USER_ZDOTDIR
else
  unset ZDOTDIR
fi
unset FORGETTY_USER_ZDOTDIR
[[ -r ${ZDOTDIR:-$HOME}/.zshenv ]] && builtin source "${ZDOTDIR:-$HOME}/.zshenv"
if [[ -o interactive && -r ${FORGETTY_SHELL_INTEGRATION_DIR-}/forgetty.zsh ]]; then
  _forgetty_bootstrap() {
    add-zsh-hook -d precmd _forgetty_bootstrap
    unfunction _forgetty_bootstrap
    builtin source "$FORGETTY_SHELL_INTEGRATION_DIR/forgetty.zsh"
  }
  autoload -Uz add-zsh-hook
  add-zsh-hook precmd _forgetty_bootstrap
fi
