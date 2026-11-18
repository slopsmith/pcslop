# Runs in the Ubuntu 8.04 live session; scripts/bench-cpu reads the BENCH
# lines from the serial console. Each command also prints a checksum, which
# must match bench/ubuntu/expected.
TIMEFORMAT=%R
d=$(dirname "$0")
run() {
    t=$( { time eval "$2" >"/tmp/out.$1"; } 2>&1 )
    echo "BENCH $1 $t $(head -c 40 "/tmp/out.$1" | tr -s ' \n' ' ')"
}
run py "python $d/loop.py"
run objs "python $d/objs.py"
run perl "perl -e '\$s=0; for \$i (1..2000000) {\$s+=\$i%7} print \"\$s\n\"'"
run bzip2 "seq 1 1500000 | bzip2 -9 | md5sum"
run gzip "seq 1 3000000 | gzip -9n | md5sum"
run md5 "head -c 200000000 /dev/zero | md5sum"
echo BENCH done
