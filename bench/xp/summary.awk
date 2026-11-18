# Turns what the benchmark CD wrote to COM1 into the result table that
# scripts/bench-xp prints. "== NAME" lines from one.bat start each tool's
# output; bench.exe lines pass through unchanged.
function row(name, value, unit) { printf "%-16s %10s %s\n", name, value, unit }
function bad(msg) { print "BAD " msg; nbad++ }

/^== / { sec = $2; next }
/^BENCH-DONE/ { done = 1; next }

sec == "7zip" {
    if (/^(Avr|Tot):/) print
    else if (/[Ee]rror/) bad("7zip: " $0)
    next
}
sec == "coremark" {
    if (/^Iterations\/Sec/) row("coremark", $NF, "iter/s")
    else if (/ERROR!/ && !/10 secs/) bad("coremark: " $0)
    next
}
sec == "scimark" {
    if (/^Composite Score/) comp = $NF
    else if (/^FFT/) fft = $3
    else if (/^SOR/) sor = $3
    else if (/^MonteCarlo/) mc = $3
    else if (/^Sparse/) sp = $4
    else if (/^LU/) {
        row("scimark", comp, "Mflops")
        printf "  FFT %s  SOR %s  MonteCarlo %s  sparse %s  LU %s\n", fft, sor, mc, sp, $3
    }
    next
}
sec == "whetstone" {
    if (/Whetstones:/) row("whetstone", $(NF - 1), $NF == "MIPS" ? "MWIPS" : "KWIPS")
    else if (/Insufficient/) bad("whetstone: " $0)
    next
}
sec == "linpack" {
    if ($1 ~ /^[0-9]+$/ && NF == 6) kflops = $NF
    else if (/^Enter array size/ && kflops != "") { row("linpack", kflops, "KFLOPS"); kflops = "" }
    next
}
/^BAD / { nbad++; print; next }
/^check / || /^[a-z].* s / || /^total/ || /^games total/ { print }

END {
    if (kflops != "") row("linpack", kflops, "KFLOPS")
    if (nbad) { print nbad " wrong result(s)"; exit 1 }
}
