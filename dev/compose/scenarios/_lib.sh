# Shared helpers for the failure-injection scenarios.
#
# Every scenario is a small, reversible act of sabotage with a stated
# expectation. They exist so that Sentinel's diagnoses can be checked against
# faults whose cause is known exactly — which is the only way to tell a correct
# diagnosis from a plausible one.
set -euo pipefail

COMPOSE_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "${COMPOSE_DIR}"

# Roles, so a scenario can find the right supervisor socket.
role_of() {
    case "$1" in
        controller) echo controller ;;
        compute*)   echo compute ;;
        filesrv*)   echo fileserver ;;
        *) echo "unknown service: $1" >&2; return 1 ;;
    esac
}

supervisor() {
    local service="$1"; shift
    local role
    role="$(role_of "${service}")"
    docker compose exec -T "${service}" \
        supervisorctl -c "/etc/supervisor/roles/${role}.conf" "$@"
}

in_container() {
    local service="$1"; shift
    docker compose exec -T "${service}" "$@"
}

require_service() {
    local service="$1"
    if ! docker compose ps --services 2>/dev/null | grep -qx "${service}"; then
        echo "no such service: ${service}" >&2
        echo "available: $(docker compose ps --services | tr '\n' ' ')" >&2
        exit 64
    fi
}

announce() {
    echo "=== $* ==="
}

expect() {
    echo
    echo "Expected diagnosis:"
    for line in "$@"; do
        echo "  ${line}"
    done
    echo
    echo "Check with: ./scripts/sentinel status"
}
