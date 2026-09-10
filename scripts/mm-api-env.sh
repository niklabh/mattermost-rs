# The environment `mm-api` must run with to agree with the Go server beside it.
#
#   source "$ROOT/scripts/mm-api-env.sh"
#   mmrs_launch_mm_api /path/to/logfile
#
# `scripts/go-server.sh` sets several settings as **environment overrides**, and an override
# never reaches the configuration document in the database — which is the document `mm-api`
# reads. So without these the two servers disagree about configuration they are both supposed to
# share, and a route gated on one of them answers differently on each side for a reason that has
# nothing to do with the port. `MM_FILESETTINGS_DIRECTORY` was the first (a 404 for every file
# that exists); `MM_TEAMSETTINGS_ENABLEOPENSERVER` is the second, and it is the difference
# between a 400 and a page of users on `GET /api/v4/users/invalid_emails`.
#
# **Scoped to the launch and never exported.** `cargo test` runs in the same shell, and
# `config::go_parity::the_env_overlay_preserves_document_values_it_does_not_name` asserts that no
# `MM_` variable is set — an export here fails a unit test that has nothing to do with the code
# it covers.
#
# Both launch sites source this. They used to differ: `parity.sh` set the file directory and
# `mutate.sh` did not, so every `api`-suite mutation ran against a server configured unlike the
# one the tests were written against.
mmrs_launch_mm_api() {
  local log="${1:-/tmp/mmrs-mm-api.log}"
  local root="${MMRS_ROOT:-$(cd "$(dirname "${BASH_SOURCE[0]:-$0}")/.." && pwd)}"
  (
    MM_FILESETTINGS_DIRECTORY="$root/reference/.build/mmroot/data/" \
    MM_TEAMSETTINGS_ENABLEOPENSERVER=true \
    nohup "$root/target/debug/mm-api" > "$log" 2>&1 &
  )
}
