#!/usr/bin/env bash
set -euo pipefail

if [[ "${1:-}" == "--env-file-if-exists" ]]; then
  shift 2
fi

case " $* " in
  *" repl "*)
    echo "$$" >> "$TEST_ROOT/repl-pids"
    echo "adapter-output-during-repl" >> "$TEST_ROOT/.exo/exo-adapters.log"
    echo "scheduler-output-during-repl" >> "$TEST_ROOT/.exo/exo-scheduler.log"
    echo "repl-ready"
    while read -r command; do
      case "$command" in
        restart)
          touch "$TEST_ROOT/.exo/exo-control.restart"
          ;;
        finish)
          # Give a competing log tail time to expose output interference.
          sleep 2
          exit 7
          ;;
      esac
    done
    ;;
  *" adapters run "*|*" run --watch "*)
    echo "service-started"
    ;;
esac
