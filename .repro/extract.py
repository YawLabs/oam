import re
src = open('crates/oam_cli/tests/e2e.rs', encoding='utf-8').read()


def grab(name):
    m = re.search(r'const ' + name + r': &str = "(.*?)";', src, re.S)
    s = m.group(1)
    # First join Rust line continuations (backslash + newline + indent).
    s = re.sub(r'\\\s*\n\s*', '', s)
    # Then the remaining \n escapes are real newlines.
    s = s.replace('\\n', '\n')
    return s


for n, f in [('TLS_TEST_CA_CERT', '.repro/ca.pem'),
             ('TLS_TEST_LEAF_CERT', '.repro/leaf.pem'),
             ('TLS_TEST_LEAF_KEY', '.repro/leaf.key')]:
    v = grab(n)
    open(f, 'w', newline='\n').write(v + '\n')
    print(n, len(v), '|', v.splitlines()[0], '|', v.splitlines()[-1])
