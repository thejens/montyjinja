# Sales report — {{ year }}

{% set s = summarize(regions) -%}
Grand total: **{{ s.grand_total|money }}** · best region: **{{ s.best }}**

## Regions
{% for region in regions %}
{{ region_card(region) }}  change {{ region.monthly|pct_change }}
{%- set totals = regions|map(attribute='monthly')|map('sum')|list %}
{%- if region.monthly|sum is outlier(totals) %} ⚠ outlier{% endif %}
{%- endfor %}

## Ranking (inline `py()`)

{{ py("', '.join(f'{i}. {name}' for i, name in enumerate(s['ranking'], 1))") }}

## Months above target (`{% raw %}{% python %}{% endraw %}`)

{% python target=target -%}
months = ['Jan', 'Feb', 'Mar', 'Apr', 'May', 'Jun']
for region in regions:
    hits = [m for m, v in zip(months, region['monthly']) if v >= target]
    print(f"- {region['name']}: {', '.join(hits) or 'none'}")
{%- endpython %}
