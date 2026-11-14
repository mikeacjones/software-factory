#!/usr/bin/env bash
# Back up every bead database on the Dolt server, and prove each backup restores.
#
#   proofs/backup_beads.sh backup         take a new backup set, then verify it
#   proofs/backup_beads.sh verify [DIR]   restore a backup set (default: the latest)
#                                         and compare it with its manifest
#
# The server is only read. `dolt_backup('sync-url', ...)` copies a database,
# working set included, to a directory and changes nothing in it: no backup is
# registered in the database's own config. Restores go to a temp directory
# outside the server, never to a database on it.
#
# Settings (environment):
#   DOLT_HOST, DOLT_PORT, DOLT_USER   server to back up (127.0.0.1, 3306, root)
#   DOLT_DATA_DIR                     the server's data directory; backups are
#                                     refused inside it (/opt/homebrew/var/dolt)
#   FACTORY_BACKUP_ROOT               where backup sets go
#                                     (~/.local/share/software-factory/backups/beads)
#
# A backup set is <root>/<UTC timestamp>/ holding one directory per database
# and manifest.tsv: database, HEAD commit, database hash, row count of issues.
#
# Exit 0 only if every database was backed up and every backup verified.
set -euo pipefail

DOLT_HOST="${DOLT_HOST:-127.0.0.1}"
DOLT_PORT="${DOLT_PORT:-3306}"
DOLT_USER="${DOLT_USER:-root}"
DOLT_DATA_DIR="${DOLT_DATA_DIR:-/opt/homebrew/var/dolt}"
BACKUP_ROOT="${FACTORY_BACKUP_ROOT:-$HOME/.local/share/software-factory/backups/beads}"

die() { echo "backup_beads: $*" >&2; exit 1; }

server() {
  dolt --host "$DOLT_HOST" --port "$DOLT_PORT" --user "$DOLT_USER" --password "" --no-tls "$@"
}

# One value from a one-row, one-column query: the last line of CSV output.
value() { tail -n 1 | tr -d '\r'; }

# Print "<HEAD commit> <database hash> <issues row count>" for a database.
# The first argument says how to reach it: "server" or a restored directory.
facts() {
  local where="$1" db="$2" q head hash has count
  q() {
    if [ "$where" = server ]; then
      server --use-db "$db" sql -r csv -q "$1"
    else
      (cd "$where" && dolt sql -r csv -q "$1")
    fi
  }
  head="$(q "select dolt_hashof('HEAD')" | value)"
  hash="$(q "select dolt_hashof_db()" | value)"
  has="$(q "select count(*) from information_schema.tables where table_schema = database() and table_name = 'issues'" | value)"
  if [ "$has" = 1 ]; then
    count="$(q "select count(*) from issues" | value)"
  else
    count="na"
  fi
  echo "$head $hash $count"
}

outside_data_dir() {
  case "$1" in
    "$DOLT_DATA_DIR" | "$DOLT_DATA_DIR"/*) die "$1 is inside the server's data directory" ;;
    /*) ;;
    *) die "$1 is not an absolute path" ;;
  esac
}

# The bead databases on the server: everything but Dolt's own two.
server_databases() {
  local dbs
  dbs="$(server sql -r csv -q "show databases" | tail -n +2 | tr -d '\r' | grep -v -x -e information_schema -e mysql || true)"
  [ -n "$dbs" ] || die "the server at $DOLT_HOST:$DOLT_PORT lists no databases"
  echo "$dbs"
}

verify_set() {
  local set="$1" scratch failed=0 db head hash count got
  [ -f "$set/manifest.tsv" ] || die "no manifest in $set"
  outside_data_dir "$set"
  # Every database on the server now must be in this set.
  while IFS= read -r db; do
    if ! cut -f 1 "$set/manifest.tsv" | grep -q -x -F -e "$db"; then
      echo "FAIL $db: on the server but not in $set"
      failed=1
    fi
  done <<<"$(server_databases)"
  scratch="$(mktemp -d "${TMPDIR:-/tmp}/backup_beads.XXXXXX")"
  # shellcheck disable=SC2064
  trap "rm -rf '$scratch'" EXIT
  # A restore records an author. Give it a throwaway identity in the scratch
  # directory, so the owner's own Dolt config is neither needed nor changed.
  export DOLT_ROOT_PATH="$scratch/home"
  mkdir "$DOLT_ROOT_PATH"
  dolt config --global --add user.name "backup verify" >/dev/null
  dolt config --global --add user.email "backup-verify@localhost" >/dev/null
  while IFS=$'\t' read -r db head hash count; do
    [ -n "$db" ] || continue
    if ! (cd "$scratch" && dolt backup restore "file://$set/$db" "$db") >"$scratch/$db.log" 2>&1; then
      echo "FAIL $db: restore failed: $(tail -n 1 "$scratch/$db.log")"
      failed=1
      continue
    fi
    got="$(facts "$scratch/$db" "$db")"
    if [ "$got" = "$head $hash $count" ]; then
      echo "ok   $db: restored, issues=$count head=$head"
    else
      echo "FAIL $db: manifest says '$head $hash $count', restore has '$got'"
      failed=1
    fi
  done <"$set/manifest.tsv"
  rm -rf "$scratch"
  trap - EXIT
  return "$failed"
}

latest_set() {
  local newest
  newest="$(find "$BACKUP_ROOT" -mindepth 1 -maxdepth 1 -type d -name '20*Z' 2>/dev/null | sort | tail -n 1)"
  [ -n "$newest" ] || die "no backup set under $BACKUP_ROOT"
  echo "$newest"
}

backup() {
  local stamp set db before after tries
  outside_data_dir "$BACKUP_ROOT"
  case "$BACKUP_ROOT" in
    *"'"*) die "backup root must not contain a single quote" ;;
  esac
  stamp="$(date -u +%Y%m%dT%H%M%SZ)"
  set="$BACKUP_ROOT/$stamp"
  mkdir -p "$BACKUP_ROOT"
  chmod 700 "$BACKUP_ROOT"
  mkdir "$set"

  local dbs
  dbs="$(server_databases)"

  while IFS= read -r db; do
    # A write that lands while the copy runs would make the manifest describe a
    # state the backup does not hold, so the facts must be equal on both sides.
    tries=0
    while :; do
      tries=$((tries + 1))
      before="$(facts server "$db")"
      rm -rf "${set:?}/$db"
      server --use-db "$db" sql -q "call dolt_backup('sync-url', 'file://$set/$db')" >/dev/null
      after="$(facts server "$db")"
      [ "$before" = "$after" ] && break
      [ "$tries" -lt 3 ] || die "$db kept changing during the backup; gave up after $tries tries"
    done
    printf '%s\t%s\t%s\t%s\n' "$db" $before >>"$set/manifest.tsv"
    echo "backed up $db to $set/$db"
  done <<<"$dbs"

  verify_set "$set" || die "backup set $set did not verify"
  echo "backup set: $set"
}

case "${1:-}" in
  backup) backup ;;
  verify) verify_set "${2:-$(latest_set)}" ;;
  *) die "usage: $0 backup | verify [DIR]" ;;
esac
