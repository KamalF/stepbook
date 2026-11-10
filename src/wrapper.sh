# The wrapper of a run (spec 8.2, 9.2). It sources the block script in
# this shell, so that export, cd and shell options work as in a terminal,
# and it writes the session state when the shell exits.
#
# Usage: bash --noprofile --norc wrapper.sh SCRIPT STATE_FILE
#
# bash runs the EXIT trap after the last line, after exit, after a failure
# under set -e, and on SIGINT, SIGTERM and SIGHUP, and the trap keeps the
# exit status. The names of the wrapper start with __stepbook_, and its
# variables are not exported, so that the state never holds them.
__stepbook_script=$1
__stepbook_state=$2
__stepbook_pid=$BASHPID
# The EXIT handler of the script, which the wrapper runs from its own.
__stepbook_trap=

# Writes the directory, each exported variable as NAME=value, then an
# empty entry, each ended by a NUL byte. ">|" writes even under noclobber.
__stepbook_save() {
    set +a
    local IFS=$' \t\n' __stepbook_name
    {
        builtin printf '%s\0' "$PWD"
        for __stepbook_name in $(builtin compgen -e); do
            builtin printf '%s=%s\0' "$__stepbook_name" "${!__stepbook_name}"
        done
        builtin printf '\0'
    } >|"$__stepbook_state"
}

__stepbook_status() {
    return "$1"
}

# Saves the state, then runs the handler of the script with the exit
# status in $?, then saves the state again: the handler can change it.
# The handler can also end with exit, or with a failure under set -e,
# which skip the rest of this function.
__stepbook_exit() {
    local __stepbook_code=$?
    __stepbook_save
    if [ -z "$__stepbook_trap" ]; then
        return
    fi
    # Under set -e, a status other than 0 would end the trap here, unless
    # a "||" follows it. set -e still applies to the handler, as it would
    # without the wrapper.
    if [ "$__stepbook_code" -eq 0 ]; then
        eval "$__stepbook_trap"
    else
        __stepbook_status "$__stepbook_code" || eval "$__stepbook_trap"
    fi
    __stepbook_save
}

# Keeps the EXIT handler of the script apart, so that the wrapper still
# saves the state. A subshell runs its own EXIT trap.
trap() {
    if [ "$BASHPID" != "$__stepbook_pid" ] || [ $# -eq 0 ]; then
        builtin trap "$@"
        return
    fi
    case $1 in
    --) shift ;;
    -) ;;
    -*)
        builtin trap "$@"
        return
        ;;
    esac
    if [ $# -eq 0 ]; then
        builtin trap
        return
    fi
    # "trap SIGNAL" resets the signal, as "trap - SIGNAL" does.
    local __stepbook_action=- __stepbook_signal __stepbook_rest=()
    if [ $# -gt 1 ]; then
        __stepbook_action=$1
        shift
    fi
    for __stepbook_signal; do
        case ${__stepbook_signal^^} in
        EXIT | 0 | SIGEXIT)
            if [ "$__stepbook_action" = - ]; then
                __stepbook_trap=
            else
                __stepbook_trap=$__stepbook_action
            fi
            ;;
        *) __stepbook_rest+=("$__stepbook_signal") ;;
        esac
    done
    if [ ${#__stepbook_rest[@]} -gt 0 ]; then
        builtin trap -- "$__stepbook_action" "${__stepbook_rest[@]}"
    fi
}

builtin trap __stepbook_exit EXIT
set --
. "$__stepbook_script"
