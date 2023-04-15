#!/bin/sh

set -xe

cd /workspaces
git clone https://github.com/nakedible-p/snapview-tokio-tungstenite/
git clone https://github.com/nakedible-p/snapview-tungstenite-rs/
git clone https://github.com/nakedible-p/hyperium-headers/

cd snapview-tokio-tungstenite
git remote add upstream https://github.com/snapview/tokio-tungstenite/
git fetch upstream master
git remote add upstream-deflate https://github.com/kazk/tokio-tungstenite/
git fetch upstream-deflate feature/permessage-deflate
cd ..

cd snapview-tungstenite-rs
git remote add upstream https://github.com/snapview/tungstenite-rs/
git fetch upstream master
git fetch upstream permessage-deflate
cd ..

cd hyperium-headers
git remote add upstream https://github.com/hyperium/headers
git fetch upstream master
git remote add upstream-deflate https://github.com/kazk/headers
git fetch upstream-deflate sec-websocket-extensions
cd ..
