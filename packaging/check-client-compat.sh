#!/usr/bin/env bash
# Checks a release of the OneDrive Client for Linux still has everything this app relies on:
# the command-line options it passes, the config settings it writes, the files it reads, and
# the messages it looks for in the client's output (status, activity, previews, sign-in).
#
#   packaging/check-client-compat.sh <path to an unpacked abraunegg/onedrive source tree>
#
# Run weekly against the newest release by .github/workflows/client-compat.yml, which opens an
# issue if anything goes missing. Keep these lists in step with the code.
set -uo pipefail
SRC="${1:?usage: $0 <onedrive source dir>}"

options=(--confdir --monitor --sync --resync --resync-auth --dry-run --verbose --logout --reauth --version)
config_keys=(sync_dir drive_id disable_permission_set application_id azure_ad_endpoint azure_tenant_id
    monitor_interval rate_limit use_recycle_bin disable_notifications skip_dotfiles skip_file sync_root_files)
files=(sync_list refresh_token .config.hash .sync_list.hash .config.backup)
messages=(
    "Starting a sync with Microsoft OneDrive"          # status: syncing
    "Sync with Microsoft OneDrive is complete"         # status: up to date
    "Uploading new file"                               # activity
    "Uploading modified file"
    "Downloading file"
    "Deleting local file"
    "Deleting local directory"
    "Successfully deleted item from Microsoft OneDrive"
    "Created local directory"
    "Number of items to download from Microsoft OneDrive" # resync preview
    "New items to upload to Microsoft OneDrive"
    "Deleting item from Microsoft OneDrive"
    "requires authentication"                          # status: signed out
    "issue a --reauth and re-authorise this client"
    "Paste redirect URI here"                          # sign-in without a browser hand-back
    "https://login.microsoftonline.com"                # sign-in address
)

missing=()
found() { grep -rqF -- "$1" "$SRC/src" "$SRC/config" 2>/dev/null; }
for o in "${options[@]}"; do found "\"${o#--}" || missing+=("command-line option $o"); done
for k in "${config_keys[@]}"; do found "\"$k\"" || missing+=("config setting $k"); done
for f in "${files[@]}"; do found "$f" || missing+=("file $f"); done
for m in "${messages[@]}"; do found "$m" || missing+=("message \"$m\""); done

if [ ${#missing[@]} -eq 0 ]; then
    echo "OK: everything the app relies on is still there."
    exit 0
fi
echo "These are no longer in the client's source:"
printf -- '- %s\n' "${missing[@]}"
exit 1
