# Starting programs with a >> handle that another program is writing through at the same time
# doesn't move its position back over that program's output.
t=$(mktemp -d)
cd "$t" || exit 1
{
  /usr/bin/awk 'BEGIN { for (i = 1; i <= 100000; i++) { print "line " i; fflush() } }' &
  until [ -s m ]; do :; done
  for i in $(seq 1 40); do /usr/bin/echo "job $i" & done
  wait
} >> m
echo "lines: $(grep -c '^line [0-9]*$' m), jobs: $(grep -c '^job [0-9]*$' m), other: $(grep -vc -e '^line [0-9]*$' -e '^job [0-9]*$' m)"
echo "distinct lines: $(grep '^line' m | sort -u | wc -l)"
cd / && rm -rf "$t"
