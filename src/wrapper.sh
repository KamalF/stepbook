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
# then turns set -x, set -v, set -a and set -T off, with no trace. One line:
# set -v shows it at the exit. Under set -T, the functions of the wrapper
# and their command substitutions would run the DEBUG trap of the script,
# whose output would mix with what they read.
__stepbook_handler='{ __stepbook_code=$? __stepbook_flags=$-; set +axvT; } 2>/dev/null; __stepbook_exit'
# The EXIT handler of the script, which the wrapper runs from its own.
__stepbook_trap=

# Writes the directory, each exported variable as NAME=value, then an
# empty entry, each ended by a NUL byte. ">|" writes even under noclobber.
# Under set -x, bash would print each value, secrets included: the callers
# turn set -x off first.
__stepbook_save() {
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
    # A subshell, since it unsets variables to read the global values.
    (
        builtin trap - ERR
        set +eEu
        # Under localvar_unset, an unset would leave the local unset, with
        # the global variable still hidden.
        shopt -u localvar_unset
        # mapfile splits on line feeds, whatever IFS the script exported.
        mapfile -t __stepbook_names <<<"$(builtin compgen -e)"
        builtin printf '%s\0' "${PWD-}"
        for __stepbook_name in "${__stepbook_names[@]}"; do
            # Under set -a, bash exports the variables of the wrapper, and
            # the BASH_TRAPSIG that it sets in a trap.
            case $__stepbook_name in '' | __stepbook_* | BASH_TRAPSIG) continue ;; esac
            # After an exit in a function, or a signal, bash runs the EXIT
            # trap in that function: its locals, and those of its callers,
            # hide the global variables. The trace attribute marks the
            # global variable, and each unset removes the innermost local.
            builtin declare -g -t -- "$__stepbook_name" 2>/dev/null
            for ((__stepbook_i = 0; __stepbook_i < 2 * ${#FUNCNAME[@]} + 2; __stepbook_i++)); do
                case ${!__stepbook_name@a} in *t*) break ;; esac
                builtin unset -v -- "$__stepbook_name" 2>/dev/null || break
            done
            # bash puts no array in the environment, but compgen -e lists an
            # exported array: the state would get its first element as a
            # string.
            case ${!__stepbook_name@a} in *[aA]*) continue ;; *x*) ;; *) continue ;; esac
            if [[ -v $__stepbook_name ]]; then
                builtin printf '%s=%s\0' "$__stepbook_name" "${!__stepbook_name}"
            fi
        done
        builtin printf '\0'
    ) >|"$__stepbook_state"
}

__stepbook_status() {
    return "$1"
}

# The process substitutions of the script, such as the tee of the old
# logging header (spec 12.3), can still have output to write: the run ends
# when the wrapper exits. So the wrapper first closes its files, so that
# they read the end of their input, and waits up to 1 s for its children to
# end. A background job, which keeps them open, costs that second. Then the
# jobs keep running (spec 8.2), the children left and the processes that
# their parents left, such as the job of "(cmd &)".
#
# The shell exits right after, so the drain drops the options and the
# traps of the script, which would act on its lines: set -f would keep the
# glob from expanding, an ERR trap under set -E would run on the reads that
# fail on purpose, and in POSIX mode a failed exec would end the shell.
__stepbook_drain() {
    builtin trap - ERR DEBUG RETURN
    set +eEfuT +o posix
    shopt -u failglob nullglob
    local __stepbook_fd __stepbook_children __stepbook_tries=20
    for __stepbook_fd in /proc/"$BASHPID"/fd/*; do
        __stepbook_fd=${__stepbook_fd##*/}
        # bash reads the wrapper on fd 255. The glob read the directory on
        # an fd that is closed by now.
        if [ "$__stepbook_fd" != 255 ] && [ -e "/proc/$BASHPID/fd/$__stepbook_fd" ]; then
            exec {__stepbook_fd}>&- 2>/dev/null
        fi
    done
    if exec {__stepbook_fd}<>"$__stepbook_timer"; then
        while [ "$__stepbook_tries" -gt 0 ]; do
            __stepbook_children=
            # The file has no final line feed: read fails, and reads it all.
            read -r __stepbook_children <"/proc/$BASHPID/task/$BASHPID/children" || :
            if [ -z "$__stepbook_children" ]; then
                break
            fi
            read -r -t 0.05 -u "$__stepbook_fd" || :
            __stepbook_tries=$((__stepbook_tries - 1))
        done
    fi
    __stepbook_keep_jobs
}

# When the wrapper exits, the kernel sends SIGHUP to the process group in
# the foreground of its PTY, since the wrapper leads the session: that is
# the group of the wrapper, background jobs included. So the wrapper hands
# the PTY to a group that is gone by then: a bash with job control takes it
# for a group of its own, and dies before it can give it back. Only while
# the group of the wrapper has the PTY: otherwise that bash would stop the
# group with SIGTTIN, to wait for its turn.
__stepbook_keep_jobs() {
    local __stepbook_stat
    read -r __stepbook_stat <"/proc/$BASHPID/stat" || return 0
    # After the name of the program: state, ppid, pgrp, session, tty, and
    # the group in the foreground of the tty.
    [[ ${__stepbook_stat##*) } =~ ^[^\ ]+\ [0-9]+\ ([0-9]+)\ [0-9]+\ [0-9]+\ ([0-9]+)\  ]] || return 0
    [ "${BASH_REMATCH[1]}" = "${BASH_REMATCH[2]}" ] || return 0
    # Its status, 137, would come back to the EXIT trap, where an ERR trap
    # of the script set without set -E would fire on it.
    (exec -c /proc/self/exe -m -c 'kill -KILL $$') 2>/dev/tty || :
}

# While the handler of the script runs: its exit would skip the second
# save and the drain. In POSIX mode, bash refuses a function named exit,
# and finds its builtin first anyway. Without N, bash gives the status from
# before the trap, but in a "( )" subshell that of the last command: the
# function gives $? back before it calls the builtin.
__stepbook_exit_function='exit() {
    { local __stepbook_status=$?; set +axvT; } 2>/dev/null
    if [ "$BASHPID" = "$__stepbook_pid" ]; then
        __stepbook_save
        __stepbook_drain
    fi
    if [ "$__stepbook_status" -eq 0 ]; then
        builtin exit "$@"
    fi
    __stepbook_status "$__stepbook_status" || builtin exit "$@"
}'

# Saves the state, then runs the handler of the script with the exit
# status in $?, and with set -a as the script left it, then saves the
# state again: the handler can change it. If the handler ends with exit,
# the exit function saves and drains first. A failure under set -e in the
# handler skips the rest: the second save and the drain (spec 8.2). The
# handler runs with set -x and set -T off, as the wrapper does. They go off
# again after it, since the handler can turn them on.
__stepbook_exit() {
    __stepbook_save
    if [ -n "$__stepbook_trap" ]; then
        if ! shopt -oq posix; then
            eval "$__stepbook_exit_function"
        fi
        case $__stepbook_flags in *a*) set -a ;; esac
        # Under set -e, a status other than 0 would end the trap here,
        # unless a "||" follows it. set -e still applies to the handler, as
        # it would without the wrapper.
        if [ "$__stepbook_code" -eq 0 ]; then
            eval "$__stepbook_trap"
        else
            __stepbook_status "$__stepbook_code" || eval "$__stepbook_trap"
        fi
        { set +axvT; } 2>/dev/null
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
    # Under set -x or set -v, the trace would show the wrapper, and under
    # set -T, the DEBUG trap of the script would run in the command
    # substitutions that read the slot. "local -" gives the flags back at
    # the return.
    { local -; set +xvT; } 2>/dev/null
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
