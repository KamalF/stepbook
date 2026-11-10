# The wrapper of a run (spec 8.2, 9.2). It sources the block script in
# this shell, so that export, cd and shell options work as in a terminal,
# and it writes the session state when the shell exits.
#
# Usage: bash --noprofile --norc wrapper.sh SCRIPT STATE_FILE
#
# bash runs the EXIT trap after the last line, after exit, after a failure
# under set -e, and on SIGINT, SIGTERM and SIGHUP, and the trap keeps the
# exit status. The names of the wrapper start with __stepbook_.
__stepbook_script=$1
__stepbook_state=$2
__stepbook_pid=$BASHPID
# The EXIT handler of the wrapper. It keeps the exit status and the flags,
# then turns set -x and set -a off, with no trace.
__stepbook_handler='{ __stepbook_code=$? __stepbook_flags=$-; set +ax; } 2>/dev/null
__stepbook_exit'
# The EXIT handler of the script, which the wrapper runs from its own.
__stepbook_trap=

# Writes the directory, each exported variable as NAME=value, then an
# empty entry, each ended by a NUL byte. ">|" writes even under noclobber.
# Under set -x, bash would print each value, secrets included: the trap
# turns set -x off first.
__stepbook_save() {
    local IFS=$' \t\n' __stepbook_name
    {
        builtin printf '%s\0' "$PWD"
        for __stepbook_name in $(builtin compgen -e); do
            # Under set -a, bash exports the variables of the wrapper, and
            # the BASH_TRAPSIG that it sets in a trap.
            case $__stepbook_name in __stepbook_* | BASH_TRAPSIG) continue ;; esac
            builtin printf '%s=%s\0' "$__stepbook_name" "${!__stepbook_name}"
        done
        builtin printf '\0'
    } >|"$__stepbook_state"
}

__stepbook_status() {
    return "$1"
}

# Saves the state, then runs the handler of the script with the exit
# status in $?, and with set -a as the script left it, then saves the
# state again: the handler can change it. The handler can also end with
# exit, or with a failure under set -e, which skip the rest of this
# function. set -x stays off, since its trace would show the wrapper.
__stepbook_exit() {
    __stepbook_save
    if [ -z "$__stepbook_trap" ]; then
        return
    fi
    case $__stepbook_flags in *a*) set -a ;; esac
    # Under set -e, a status other than 0 would end the trap here, unless
    # a "||" follows it. set -e still applies to the handler, as it would
    # without the wrapper.
    if [ "$__stepbook_code" -eq 0 ]; then
        eval "$__stepbook_trap"
    else
        __stepbook_status "$__stepbook_code" || eval "$__stepbook_trap"
    fi
    set +a
    __stepbook_save
}

# Sets __stepbook_slot to the handler in the EXIT slot of bash. In a
# subshell that has set no trap, bash shows the traps of its parent.
__stepbook_read_slot() {
    eval "set -- $(builtin trap -p EXIT)"
    __stepbook_slot=${3-}
}

# The script sees its own handler in the EXIT slot: bash parses each form
# of trap, and answers each query, with the handler of the script in place
# of the wrapper's. So "trap -p EXIT" and "$(trap -p EXIT)" give the
# handler of the script, and a handler that chains them never calls the
# wrapper's.
trap() {
    local __stepbook_slot __stepbook_rc=0
    __stepbook_read_slot
    # A subshell that set its own EXIT trap.
    if [ "$__stepbook_slot" != "$__stepbook_handler" ]; then
        builtin trap "$@"
        return
    fi
    if [ -n "$__stepbook_trap" ]; then
        builtin trap -- "$__stepbook_trap" EXIT
    else
        builtin trap - EXIT
    fi
    # Under set -e, a failure here would end the script with the handler
    # of the script in the slot: the state would not be saved.
    builtin trap "$@" || __stepbook_rc=$?
    __stepbook_read_slot
    if [ "$BASHPID" = "$__stepbook_pid" ]; then
        __stepbook_trap=$__stepbook_slot
        builtin trap -- "$__stepbook_handler" EXIT
    elif [ "$__stepbook_slot" = "$__stepbook_trap" ]; then
        # A subshell that did not set an EXIT trap has none.
        builtin trap - EXIT
    fi
    return "$__stepbook_rc"
}

builtin trap -- "$__stepbook_handler" EXIT
set --
. "$__stepbook_script"
