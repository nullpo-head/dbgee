#!/bin/sh

set -e

pid=
is_pid=0
is_user_name=0  # for fake sudo
for arg in "$@"; do
  if [ "$arg" = -p ]; then
    is_pid=1
  elif [ "$is_pid" = 1 ]; then
    is_pid=0
    pid="$arg"
    # print <NUM> to enable assertion with the output
    printf "'<NUM>' "
    continue
  elif [ "$arg" = -u ]; then
    is_user_name=1
  elif [ "$is_user_name" = 1 ]; then
    is_user_name=0
    # print <USER> to enable assertion with the output
    printf "'<USER>' "
    continue
  fi

  printf "'$arg' "
done

printf '\n'


# Wake the sleeping command.
# the command sleeping may need multiple SIGCONT
kill -s CONT "$pid" || true
kill -s CONT "$pid" || true
kill -s CONT "$pid" || true
