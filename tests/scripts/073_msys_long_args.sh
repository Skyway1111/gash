# MSYS programs get argument lists longer than a Windows command line (32,767 characters), as in
# Git Bash, where the MSYS runtime passes them on without the command line (#20).
t=$(mktemp -d)
cd "$t" || exit 1
for i in $(seq 1 1200); do : > "file-with-a-moderately-long-name-$i.txt"; done
set -- file-*.txt
echo "args=$# chars=$(printf '%s ' "$@" | wc -c)"
ls file-*.txt | wc -l
echo "ls: ${PIPESTATUS[0]}"
/usr/bin/cat file-*.txt > /dev/null
echo "cat: $?"
ls file-*.txt > /dev/null &
wait $!
echo "background ls: $?"

# Quoting survives: spaces, quotes, backslashes, a leading @, glob characters, an empty argument.
long=$(printf 'x%.0s' $(seq 1 40000))
/usr/bin/printf '[%s]\n' "$long" "a b" "it's" 'back\slash' '"quoted"' "@at" '*' '' | cut -c1-14

# A native program still hits the Windows limit, as in Git Bash.
cmd.exe /d /c "exit 0" "$long" 2>/dev/null
echo "native: $?"

cd / && rm -rf "$t"
