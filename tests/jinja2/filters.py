# Filters, tests and globals registered the way Python Jinja2 does it.
# The golden generator runs this file under CPython with a real
# jinja2.Environment as `env`; montyjinja runs it unchanged in Monty.


def money(amount, currency='EUR'):
    symbol = {'EUR': '€', 'USD': '$'}.get(currency, currency + ' ')
    return f'{symbol}{amount:,.2f}'


def slugify(text):
    return '-'.join(text.lower().split())


def initials(name):
    return ''.join(part[0].upper() for part in name.split())


def is_prime(n):
    return n > 1 and all(n % d for d in range(2, n))


def stats(values):
    total = sum(values)
    return {'n': len(values), 'total': total, 'mean': total / len(values)}


env.filters['money'] = money
env.filters['slugify'] = slugify
env.filters['initials'] = initials
env.filters['shout'] = lambda text, times=1: text.upper() + '!' * times
env.tests['prime'] = is_prime
env.globals['stats'] = stats
env.globals['SITE'] = 'example.com'
env.globals['LIMITS'] = {'max': 10, 'min': 1}
env.filters['evens'] = lambda values: (v for v in values if v % 2 == 0)
