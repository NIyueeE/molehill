#!/bin/bash

rm -v *-mem.log

echo frp
while true; do
	ps -C frpc -o rsz= >> frpc-mem.log
sleep 1
done &

while true; do
	ps -C frps -o rsz= >> frps-mem.log
sleep 1
done &

echo GET http://127.0.0.1:5203 | vegeta attack -duration 30s -rate 1000  > /dev/null

sleep 10

kill $(jobs -p)


echo molehill

pid_s=$(ps aux | grep "molehill -s" | head -n 1 | awk '{print $2}')
while true; do
	ps --pid $pid_s -o rsz= >> molehillc-mem.log
sleep 1
done &

pid_c=$(ps aux | grep "molehill -c" | head -n 1 | awk '{print $2}')
while true; do
	ps --pid $pid_c -o rsz= >> molehills-mem.log
sleep 1
done &

echo GET http://127.0.0.1:5202 | vegeta attack -duration 30s -rate 1000 > /dev/null

sleep 10

kill $(jobs -p)

gawk -i inplace '{print $1 "000"}' frpc-mem.log
gawk -i inplace '{print $1 "000"}' frps-mem.log
gawk -i inplace '{print $1 "000"}' molehillc-mem.log
gawk -i inplace '{print $1 "000"}' molehills-mem.log
