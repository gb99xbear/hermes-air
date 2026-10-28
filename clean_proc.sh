#!/bin/bash
for p in $(pgrep -f 'hermes-rust-dev/target'); do
    kill -9 $p 2>/dev/null
done
