#!/usr/bin/env bash
set -e
cd `git rev-parse --show-toplevel`

usedocker=true

print_usage() {
  printf "Usage: ..."
}

while getopts ':dn-:' optchar; do
  case "${optchar}" in
    -)
      case "${OPTARG}" in
        no-docker)
          usedocker=false
          ;;
        nodocker)
          usedocker=false
          ;;
        *)
          if [ "$OPTERR" = 1 ] && [ "${optspec:0:1}" != ":" ]; then
            echo "Unknown option --${OPTARG}" >&2
          fi
          ;;
      esac;;
    n) usedocker=false ;;
    d) usedocker=false ;;
    *) echo "Read the source or pass no-docker to skip creating docker redis"
       exit 1 ;;
  esac
done

if [ $usedocker == true ]; then
  DOCKER_TEST_CONTAINER=$(docker run --rm -itd -p 6379:6379 redis)
  echo "Launched docker test container: $DOCKER_TEST_CONTAINER"
  trap "docker stop $DOCKER_TEST_CONTAINER 2>/dev/null 1>/dev/null && echo 'Removed test container' || true" EXIT;
else
  echo "Skipping docker test container creation"
fi
cargo test -p snocat --lib --features full,integration-redis -- integration_tests || true
