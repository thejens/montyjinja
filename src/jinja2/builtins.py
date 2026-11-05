# Jinja2 built-in filters that MiniJinja lacks, with Jinja2 3.1's behaviour.
# Loaded as an ordinary montyjinja module, registered the Jinja2 way.

import random as _random
import re


def truncate(s, length=255, killwords=False, end='...', leeway=None):
    if leeway is None:
        leeway = 5
    if length < len(end):
        raise ValueError('expected length >= len(end)')
    if leeway < 0:
        raise ValueError('expected leeway >= 0')
    if len(s) <= length + leeway:
        return s
    if killwords:
        return s[: length - len(end)] + end
    return s[: length - len(end)].rsplit(' ', 1)[0] + end


def wordcount(s):
    return len(re.findall(r'\w+', str(s)))


def center(value, width=80):
    return str(value).center(width)


def filesizeformat(value, binary=False):
    size = float(value)
    base = 1024 if binary else 1000
    prefixes = (
        ['KiB', 'MiB', 'GiB', 'TiB', 'PiB', 'EiB', 'ZiB', 'YiB']
        if binary
        else ['kB', 'MB', 'GB', 'TB', 'PB', 'EB', 'ZB', 'YB']
    )
    if size == 1:
        return '1 Byte'
    if size < base:
        return f'{int(size)} Bytes'
    unit = base
    prefix = prefixes[0]
    for i, prefix in enumerate(prefixes):
        unit = base ** (i + 2)
        if size < unit:
            return f'{base * size / unit:.1f} {prefix}'
    return f'{base * size / unit:.1f} {prefix}'


_ENTITIES = {'amp': '&', 'lt': '<', 'gt': '>', 'quot': '"', 'apos': "'", 'nbsp': '\xa0'}


def _unescape(text):
    # MarkupSafe defers to html.unescape, which Monty lacks: this covers
    # numeric references and the entities MarkupSafe itself produces.
    out = []
    i = 0
    while i < len(text):
        end = text.find(';', i) if text[i] == '&' else -1
        if end != -1:
            name = text[i + 1 : end]
            if name.startswith('#x') or name.startswith('#X'):
                out.append(chr(int(name[2:], 16)))
                i = end + 1
                continue
            if name.startswith('#') and name[1:].isdigit():
                out.append(chr(int(name[1:])))
                i = end + 1
                continue
            if name in _ENTITIES:
                out.append(_ENTITIES[name])
                i = end + 1
                continue
        out.append(text[i])
        i += 1
    return ''.join(out)


def striptags(value):
    value = str(value)
    # Comments first, so a tag inside a comment cannot end it early.
    while (start := value.find('<!--')) != -1:
        end = value.find('-->', start)
        if end == -1:
            break
        value = value[:start] + value[end + 3 :]
    while (start := value.find('<')) != -1:
        end = value.find('>', start)
        if end == -1:
            break
        value = value[:start] + value[end + 1 :]
    return _unescape(' '.join(value.split()))


def random(seq):
    seq = list(seq)
    return _random.choice(seq) if seq else None


env.filters['truncate'] = truncate
env.filters['wordcount'] = wordcount
env.filters['center'] = center
env.filters['filesizeformat'] = filesizeformat
env.filters['striptags'] = striptags
env.filters['random'] = random
