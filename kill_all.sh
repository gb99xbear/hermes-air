#!/bin/bash
ps -ef | grep 'hermes-gateway' | grep -v grep | awk '{print $2}' | xargs -r kill -9
