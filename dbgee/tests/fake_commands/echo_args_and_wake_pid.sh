#!/bin/sh

set -e

pid=
is_pid=0
for arg in "$@"; do
  if [ "$arg" = -p ]; then
    is_pid=1
  elif [ "$is_pid" = 1 ]; then
    is_pid=0
    pid="$arg"

    # the command sleeping may need multiple SIGCONT
    kill -s CONT "$pid" || true
    kill -s CONT "$pid" || true
    kill -s CONT "$pid" || true

    # print <NUM> to enable assertion with the output
    printf "'<NUM>' "
    continue
  fi

  printf "'$arg' "
done

printf '\n'
