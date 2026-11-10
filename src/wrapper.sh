# The wrapper of a run (spec 8.2, 9.2). It sources the block script in
# this shell, so that export, cd and shell options work as in a terminal,
# and it writes the session state when the shell exits.
#
# Usage: bash --noprofile --norc wrapper.sh SCRIPT STATE_FILE ENV_FILE TIMER
#
# ENV_FILE holds the large variables of the session, and TIMER is a FIFO
# that the wrapper reads with a timeout to wait.
#
# bash runs the EXIT trap after the last line, after exit, after a failure
# under set -e, and on SIGINT, SIGTERM and SIGHUP, and the trap keeps the
# exit status. The names of the wrapper start with __stepbook_.
__stepbook_script=$1
__stepbook_state=$2
__stepbook_timer=$4
__stepbook_pid=$BASHPID
# In POSIX mode, after an export of POSIXLY_CORRECT for one, bash refuses a
# function named trap: the wrapper defines its functions with POSIX mode
# off, and turns it on again before the script. "set +o posix" unsets
# POSIXLY_CORRECT, which the wrapper then gives back.
__stepbook_posix=
if shopt -oq posix; then
    __stepbook_posix=1
    __stepbook_posixly_correct=${POSIXLY_CORRECT-}
    set +o posix
fi
# The large variables of the session come in a file: in the environment of
# exec, one value of over 128 KiB fails the start of every run.
if [ -s "$3" ]; then
    while IFS= read -r -d '' __stepbook_entry; do
        export -- "$__stepbook_entry"
    done <"$3"
    unset __stepbook_entry
fi
# The EXIT handler of the wrapper. It keeps the exit status and the flags,
# then turns set -x, set -v and set -a off, with no trace. One line: set -v
# shows it at the exit.
__stepbook_handler='{ __stepbook_code=$? __stepbook_flags=$-; set +axv; } 2>/dev/null; __stepbook_exit'
# The EXIT handler of the script, which the wrapper runs from its own.
__stepbook_trap=

# Writes the directory, each exported variable as NAME=value, then an
# empty entry, each ended by a NUL byte. ">|" writes even under noclobber.
# Under set -x, bash would print each value, secrets included: the trap
# turns set -x off first.
__stepbook_save() {
    local __stepbook_names __stepbook_name __stepbook_arrays
    # Only into the file that stepbook made: after stepbook ends, its
    # workspace is gone, and a directory of the same name can be another
    # user's.
    if [ ! -f "$__stepbook_state" ] || [ ! -O "$__stepbook_state" ] || [ -L "$__stepbook_state" ]; then
        return 0
    fi
    # A bash without programmable completion has no compgen. A state with
    # no variable would remove them all from the session: no state is
    # better.
    builtin compgen -A builtin printf >/dev/null 2>&1 || return 0
    # mapfile splits on line feeds, whatever IFS the script exported.
    mapfile -t __stepbook_names <<<"$(builtin compgen -e)"
    # bash puts no array in the environment, but compgen -e lists an
    # exported array: the state would get its first element as a string.
    __stepbook_arrays=" $(builtin compgen -A arrayvar) "
    {
        builtin printf '%s\0' "${PWD-}"
        for __stepbook_name in "${__stepbook_names[@]}"; do
            # Under set -a, bash exports the variables of the wrapper, and
            # the BASH_TRAPSIG that it sets in a trap.
            case $__stepbook_name in '' | __stepbook_* | BASH_TRAPSIG) continue ;; esac
            case $__stepbook_arrays in *[[:space:]]$__stepbook_name[[:space:]]*) continue ;; esac
            builtin printf '%s=%s\0' "$__stepbook_name" "${!__stepbook_name}"
        done
        builtin printf '\0'
    } >|"$__stepbook_state"
}

__stepbook_status() {
    return "$1"
}

# The process substitutions of the script, such as the tee of the old
# logging header (spec 12.3), can still have output to write. The kernel
# sends SIGHUP to them when the wrapper exits, since it leads the session
# of the PTY. So the wrapper first closes its files, so that they read the
# end of their input, and waits up to 1 s for its children to end. A
# background job, which keeps them open, costs that second.
__stepbook_drain() {
    local - __stepbook_fd __stepbook_children __stepbook_tries=20
    set +e
    for __stepbook_fd in /proc/"$BASHPID"/fd/*; do
        __stepbook_fd=${__stepbook_fd##*/}
        # bash reads the wrapper on fd 255.
        if [ "$__stepbook_fd" != 255 ]; then
            exec {__stepbook_fd}>&- 2>/dev/null
        fi
    done
    exec {__stepbook_fd}<>"$__stepbook_timer" || return 0
    while [ "$__stepbook_tries" -gt 0 ]; do
        __stepbook_children=
        read -r __stepbook_children <"/proc/$BASHPID/task/$BASHPID/children"
        if [ -z "$__stepbook_children" ]; then
            return 0
        fi
        read -r -t 0.05 -u "$__stepbook_fd"
        __stepbook_tries=$((__stepbook_tries - 1))
    done
}

# Saves the state, then runs the handler of the script with the exit
# status in $?, and with set -a as the script left it, then saves the
# state again: the handler can change it. The handler can also end with
# exit, or with a failure under set -e, which skip the rest of this
# function. set -x stays off, since its trace would show the wrapper.
__stepbook_exit() {
    __stepbook_save
    if [ -n "$__stepbook_trap" ]; then
        case $__stepbook_flags in *a*) set -a ;; esac
        # Under set -e, a status other than 0 would end the trap here,
        # unless a "||" follows it. set -e still applies to the handler, as
        # it would without the wrapper.
        if [ "$__stepbook_code" -eq 0 ]; then
            eval "$__stepbook_trap"
        else
            __stepbook_status "$__stepbook_code" || eval "$__stepbook_trap"
        fi
        set +a
        __stepbook_save
    fi
    __stepbook_drain
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
#
# In a function, bash resets the ERR, DEBUG and RETURN traps, and puts
# them back at the return: through this function, "trap - ERR" removes
# nothing, "trap -p ERR" shows nothing, and a RETURN trap set in a function
# also runs when this function returns (spec 9.2). An alias that turned on
# set -ET first would split "cmd && trap ..." in two commands.
trap() {
    # Under set -x or set -v, the trace would show the wrapper. "local -"
    # gives the flags back at the return.
    { local -; set +xv; } 2>/dev/null
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
# In POSIX mode, bash runs its builtin trap before the function: an EXIT
# trap of the script then replaces the wrapper's, and the state is not
# saved.
if [ -n "$__stepbook_posix" ]; then
    set -o posix
    POSIXLY_CORRECT=$__stepbook_posixly_correct
    export POSIXLY_CORRECT
fi
set --
. "$__stepbook_script"
