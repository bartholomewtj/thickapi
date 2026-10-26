#!/bin/sh
DIR="$(cd "$(dirname "$0")" && pwd)"
cat "$DIR/pi_stream.jsonl"
