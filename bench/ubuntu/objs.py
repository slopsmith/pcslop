import re
t = 0
for i in xrange(60000):
    d = {"a": i, "b": [i, i + 1]}
    t += len(str(d)) + len(re.findall("[0-9]+", str(i * 31337)))
print t
