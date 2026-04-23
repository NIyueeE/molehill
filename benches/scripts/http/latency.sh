#!/bin/sh
RATE="1 1000 2000 3000 4000"
DURATION="60s"

MOLEHILL="http://127.0.0.1:5202"
FRP="http://127.0.0.1:5203"

echo warming up frp
echo GET $FRP | vegeta attack -duration 10s > /dev/null
for rate in $RATE; do
        name="frp-${rate}qps-$DURATION.bin"
        echo $name
        echo GET $FRP | vegeta attack -rate $rate -duration $DURATION > $name
        vegeta report $name
done

echo warming up molehill
echo GET $MOLEHILL | vegeta attack -duration 10s > /dev/null
for rate in $RATE; do
        name="molehill-${rate}qps-$DURATION.bin"
        echo $name
        echo GET $MOLEHILL | vegeta attack -rate $rate -duration $DURATION > $name
        vegeta report $name
done
