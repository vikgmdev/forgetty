# Forgetty shell integration for fish.
#
# Writes the standard prompt marks (OSC 133 A, B, C, D with the exit code) and
# the working directory (OSC 7) so the Forgetty server can tell a prompt from a
# running command. fish finds this file through XDG_DATA_DIRS, which the server
# sets for shells it starts. Nothing in your home is edited.
#
# fish 4 writes the prompt marks itself: then only the directory is reported.
if status is-interactive; and not set -q __forgetty_si_loaded
    set -g __forgetty_si_loaded 1

    # The server added its own data directory to XDG_DATA_DIRS: take it out again
    # so programs started from this shell do not inherit it.
    if set -q FORGETTY_SHELL_INTEGRATION_DIR
        set -l keep
        for d in (string split : -- $XDG_DATA_DIRS)
            test "$d" = "$FORGETTY_SHELL_INTEGRATION_DIR/fish-data"; or set -a keep $d
        end
        set -gx XDG_DATA_DIRS (string join : -- $keep)
    end

    function __forgetty_osc7 --on-variable PWD
        printf '\e]7;file://%s%s\a' $hostname (string escape --style=url -- $PWD | string replace -a '%2F' '/')
    end

    if test (string split . -- $version)[1] -lt 4
        function __forgetty_preexec --on-event fish_preexec
            printf '\e]133;C\a'
        end

        function __forgetty_postexec --on-event fish_postexec
            printf '\e]133;D;%s\a' $status
        end

        function __forgetty_prompt --on-event fish_prompt
            __forgetty_osc7
            printf '\e]133;A\a'
            # The prompt function is the user's, defined by now: wrap it once so
            # B ends the prompt.
            if not set -q __forgetty_wrapped
                set -g __forgetty_wrapped 1
                if functions -q fish_prompt
                    functions -c fish_prompt __forgetty_user_prompt
                    function fish_prompt
                        __forgetty_user_prompt
                        printf '\e]133;B\a'
                    end
                end
            end
        end
    else
        function __forgetty_prompt --on-event fish_prompt
            __forgetty_osc7
        end
    end
end
