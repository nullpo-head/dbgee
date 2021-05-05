#!/bin/sh

set -e

port=
is_port=0
for arg in "$@"; do
  if [ "$arg" = --listen ]; then
    is_port=1
  elif [ "$is_port" = 1 ]; then
    is_port=0
    port="$arg"
    printf "'<NUM>' "
    continue
  fi

  printf "'$arg' "
done
printf '\n'

# need to sleep to keep alive for a while to emulate the behavior of debugpy server
for arg in "$@"; do
    # exit soon because the command is `python -c debugpy` just to check the module
    if [ "$arg" = -c ]; then
        exit 0
    fi
done

# debugpy server should not exit soon
sleep 10
