#!/bin/sh

set -e
cargo build --release
scp target/release/hook hook:/var/www/hook-new
ssh hook cp /var/www/hook /var/www/hook-old
ssh hook systemctl stop hook
ssh hook cp /var/www/hook-new /var/www/hook
ssh hook setcap CAP_NET_BIND_SERVICE=+eip /var/www/hook
ssh hook systemctl start hook
