# Template extensions for the sales report example.
#
# Everything here runs inside Monty's sandbox: no filesystem, no network,
# no environment. Each call starts from a fresh interpreter state.


@template_filter
def money(amount, currency='EUR'):
    symbol = {'EUR': '€', 'USD': '$', 'GBP': '£'}.get(currency, currency + ' ')
    whole, cents = divmod(round(amount * 100), 100)
    return f'{symbol}{whole:,}.{cents:02d}'


@template_filter
def sparkline(values):
    bars = '▁▂▃▄▅▆▇█'
    lo, hi = min(values), max(values)
    span = (hi - lo) or 1
    return ''.join(bars[int((v - lo) / span * (len(bars) - 1))] for v in values)


@template_filter('pct_change')
def percent_change(values):
    first, last = values[0], values[-1]
    change = (last - first) / first * 100
    arrow = '▲' if change >= 0 else '▼'
    return f'{arrow} {abs(change):.1f}%'


@template_test
def outlier(value, values):
    mean = sum(values) / len(values)
    var = sum((v - mean) ** 2 for v in values) / len(values)
    return abs(value - mean) > 1.5 * var ** 0.5


@template_global
def summarize(regions):
    totals = {r['name']: sum(r['monthly']) for r in regions}
    best = max(totals, key=lambda name: totals[name])
    return {
        'grand_total': sum(totals.values()),
        'best': best,
        'ranking': sorted(totals, key=lambda name: -totals[name]),
    }


@template_global
def region_card(region):
    # Python calling back into MiniJinja: render a partial template.
    return render(
        'card.md',
        name=apply_filter('upper', region['name']),
        total=money(sum(region['monthly'])),
        trend=sparkline(region['monthly']),
    )
