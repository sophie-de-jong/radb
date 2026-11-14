#!/usr/bin/env bash
# Regenerate the five representative trace files in this directory.
#
# The module specification says the five graded traces are supplied with the
# handout. They were not present in this repository, so these are stand-ins with
# the same character (working set, cycle, repeated sequential scan, random,
# skewed) generated deterministically by a Lehmer LCG. If the official files are
# supplied later, drop them in as trace1..trace5.txt; the trace command does not
# change.
set -euo pipefail
cd "$(dirname "$0")"

# trace 1: uniform over 256 pages, 1000 accesses.
awk 'BEGIN{s=12345;M=65537;for(i=0;i<1000;i++){s=(s*75+74)%M;print s%256}}' > trace1.txt

# trace 2: cyclic scan of a 50-page file, 20 passes.
awk 'BEGIN{for(r=0;r<20;r++)for(i=1;i<=50;i++)print i}' > trace2.txt

# trace 3: sequential scan of an 80-page file, repeated ten times.
awk 'BEGIN{for(r=0;r<10;r++)for(i=0;i<80;i++)print i}' > trace3.txt

# trace 4: uniform over 512 pages, 2000 accesses.
awk 'BEGIN{s=4242;M=65537;for(i=0;i<2000;i++){s=(s*75+74)%M;print s%512}}' > trace4.txt

# trace 5: 90% inside a 16-page hot set, 10% cold spread over 256 pages.
awk 'BEGIN{s=999;M=65537;for(i=0;i<1000;i++){s=(s*75+74)%M;if(s%10<9)print s%16;else print 16+(s%240)}}' > trace5.txt

wc -l trace1.txt trace2.txt trace3.txt trace4.txt trace5.txt
