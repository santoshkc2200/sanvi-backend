#!/usr/bin/env python3
"""Deterministic OpenAPI style, compatibility, response replay and docs gates."""
import argparse
import datetime
import hashlib
import json
import pathlib
import re
import subprocess
import sys
import yaml

ROOT = pathlib.Path(__file__).resolve().parent.parent
METHODS = {'get', 'post', 'put', 'patch', 'delete', 'head', 'options'}
IGNORED = {'description', 'summary', 'example', 'examples', 'title', 'externalDocs', 'deprecated'}

def load(path):
    return yaml.safe_load(path.read_text(encoding='utf-8'))

def schema_name_collisions(sources):
    """Detect collisions before ApiDoc's component map can overwrite a model."""
    names = {}
    declaration = re.compile(r'#\[derive\(([^)]*)\)\]((?:\s|#\[[\s\S]*?\])*)pub (?:struct|enum) (\w+)')
    for path, source in sources:
        for derive, attributes, name in declaration.findall(source):
            if not re.search(r'\bToSchema\b', derive):
                continue
            alias = re.search(r'#\[schema\(as\s*=\s*(\w+)\)\]', attributes)
            names.setdefault(alias.group(1) if alias else name, []).append(str(path))
    # Both layers intentionally expose the same enum; the CLI regression
    # compares their direct schemas before ApiDoc merges either definition.
    return [f'component {name} requires a namespace: {", ".join(paths)}'
            for name, paths in sorted(names.items()) if len(paths) > 1 and name != 'ImpersonationMode']

def walk(value, path=''):
    yield path, value
    if isinstance(value, dict):
        for key, child in value.items():
            yield from walk(child, f'{path}/{key}')
    elif isinstance(value, list):
        for index, child in enumerate(value):
            yield from walk(child, f'{path}/{index}')

def resolve(doc, value):
    visited = set()
    while isinstance(value, dict) and '$ref' in value:
        ref = value['$ref']
        siblings = {k: v for k, v in value.items() if k != '$ref'}
        if ref in visited:
            raise ValueError(f'reference cycle: {ref}')
        visited.add(ref)
        if not ref.startswith('#/'):
            raise ValueError(f'external reference must be bundled: {ref}')
        value = doc
        for part in ref[2:].split('/'):
            value = value[part.replace('~1', '/').replace('~0', '~')]
        if isinstance(value, dict):
            value = dict(value, **siblings)
    return value

def lint(doc):
    errors, ids = [], set()
    if doc.get('openapi') != '3.1.0':
        errors.append('requires OpenAPI 3.1.0')
    for path, item in doc.get('paths', {}).items():
        if path not in {'/healthz', '/readyz', '/internal/tls/authorize'} and not path.startswith('/api/v1/'):
            errors.append(f'{path}: requires /api/v1 prefix')
        segments = re.sub(r'\{[^}]+\}', 'id', path).split('/')[1:]
        if any(not re.fullmatch(r'[a-z][a-z0-9]*(?:-[a-z0-9]+)*', s) for s in segments):
            errors.append(f'{path}: paths must be lower-case kebab-case')
        for method, operation in item.items():
            if method not in METHODS:
                continue
            identifier = operation.get('operationId', '')
            if not re.fullmatch(r'[a-z][a-z0-9_]*', identifier) or identifier in ids:
                errors.append(f'{path}: unique snake_case operationId required')
            ids.add(identifier)
            resource = path.rsplit('/', 1)[-1]
            if identifier.startswith('list_') and not resource.endswith('s') and resource not in {'history', 'media', 'usage', 'status', 'search', 'audit', 'available'}:
                errors.append(f'{path}: collection resource must be plural')
            for param in item.get('parameters', []) + operation.get('parameters', []):
                param = resolve(doc, param)
                name, schema = param.get('name'), param.get('schema', {})
                if param.get('in') != 'query':
                    continue
                if not re.fullmatch(r'[a-z][a-z0-9_]*', name or ''):
                    errors.append(f'{path}: query parameter {name} must be snake_case')
                if name in {'offset', 'page', 'page_size', 'after_id', 'cursor', 'after_created_at'}:
                    errors.append(f'{path}: cursor pagination uses after/limit')
                if name == 'limit':
                    maximum, default = schema.get('maximum'), schema.get('default')
                    if schema.get('type') != 'integer' or schema.get('minimum') != 1 or not isinstance(maximum, (int, float)) or not isinstance(default, (int, float)) or not 1 <= default <= maximum <= 500:
                        errors.append(f'{path}: limit requires bounds 1..max<=500 and in-range default')
                if name == 'after' and schema.get('type') != 'string':
                    errors.append(f'{path}: after must be a string cursor')
            for status, response in operation.get('responses', {}).items():
                for media_type, media in resolve(doc, response).get('content', {}).items():
                    if media.get('schema', {}).get('$ref', '').endswith('/ProblemDetail') and media_type != 'application/problem+json':
                        errors.append(f'{path} {status}: errors use application/problem+json')
    for path, value in walk(doc):
        if not isinstance(value, dict):
            continue
        if '$ref' in value:
            try:
                resolve(doc, value)
            except (KeyError, TypeError, ValueError) as error:
                errors.append(f'{path}: unresolved reference {value["$ref"]}: {error}')
        for name, schema in value.get('properties', {}).items():
            token_key = path.endswith('/schemas/TokenValue') and name in {'$description', '$type', '$value'}
            if not token_key and not re.fullmatch(r'[a-z][a-z0-9_]*', name):
                errors.append(f'{path}: property {name} must be snake_case')
            def integer_money(s):
                kind = s.get('type')
                return kind in ('integer', None) or kind == ['integer', 'null'] or kind == 'object' and isinstance(s.get('additionalProperties'), dict) and integer_money(s['additionalProperties'])
            if name.endswith('_minor') and name != 'is_minor' and not integer_money(schema):
                errors.append(f'{path}: minor units must be integers')
            string_type = schema.get('type') == 'string' or isinstance(schema.get('type'), list) and 'string' in schema['type']
            if name.endswith('_at') and string_type and schema.get('format') != 'date-time':
                errors.append(f'{path}/{name}: timestamps require RFC3339 date-time')
        if value.get('type') == 'string' and 'enum' in value and value.get('format') != 'uri' and any(not re.fullmatch(r'[a-z][a-z0-9_]*', str(v)) for v in value['enum']):
            errors.append(f'{path}: enum values must be snake_case')
    return errors

def breaking(old, new):
    """Conservative semantic diff; optional additions pass, restrictions do not."""
    errors = []
    def compare(left, right, path, active_refs=frozenset()):
        if isinstance(left, dict) and isinstance(right, dict):
            if '$ref' in left or '$ref' in right:
                refs = (left.get('$ref'), right.get('$ref'))
                if refs in active_refs:
                    # Stop only the recursive component expansion. Each use's
                    # sibling restrictions still belong to the public contract.
                    left = {k: v for k, v in left.items() if k != '$ref'}
                    right = {k: v for k, v in right.items() if k != '$ref'}
                else:
                    active_refs = active_refs | {refs}
                    try:
                        left, right = resolve(old, left), resolve(new, right)
                    except (KeyError, ValueError, TypeError):
                        errors.append(f'{path}: reference removed/unresolved')
                        return
            named_map = path.rsplit('/', 1)[-1] in {'properties', 'schemas', 'headers', 'responses', 'securitySchemes', 'parameters', 'paths'}
            for key in left:
                if not named_map and (key in IGNORED or key.startswith('x-')):
                    continue
                if key not in right:
                    errors.append(f'{path}/{key}: removed')
                else:
                    compare(left[key], right[key], f'{path}/{key}', active_refs)
            for key in ('required', 'security'):
                if key not in left and right.get(key):
                    errors.append(f'{path}/{key}: requirement added')
            # Schema restrictions appearing for the first time narrow accepted inputs.
            for key in ('enum', 'pattern', 'minLength', 'maxLength', 'minimum', 'maximum', 'exclusiveMinimum', 'exclusiveMaximum', 'additionalProperties', 'minItems', 'maxItems', 'uniqueItems', 'requestBody'):
                if key not in left and key in right and (key != 'requestBody' or right[key].get('required')):
                    errors.append(f'{path}/{key}: restriction added')
            if 'parameters' not in left and isinstance(right.get('parameters'), list) and any(resolve(new, p).get('required') for p in right['parameters']):
                errors.append(f'{path}: required parameter added')
        elif isinstance(left, list) and isinstance(right, list):
            if path.endswith('/parameters'):
                before = {(p.get('in'), p.get('name')): p for p in (resolve(old, p) for p in left)}
                after = {(p.get('in'), p.get('name')): p for p in (resolve(new, p) for p in right)}
                for key, param in before.items():
                    if key not in after:
                        errors.append(f'{path}/{key}: parameter removed')
                    else:
                        compare(param, after[key], f'{path}/{key}', active_refs)
                for key in after.keys() - before.keys():
                    if after[key].get('required'):
                        errors.append(f'{path}/{key}: required input added')
            elif path.endswith('/enum'):
                if any(v not in right for v in left):
                    errors.append(f'{path}: enum narrowed')
            elif path.endswith('/required'):
                if set(left) != set(right):
                    errors.append(f'{path}: requiredness changed')
            elif path.endswith('/allOf'):
                # Composed schemas compare member-by-member: an added or
                # changed member narrows the intersection and is breaking,
                # an added optional property inside a member is not. A
                # flat list inequality would flag every additive Problem
                # Detail extension as breaking.
                if len(right) < len(left):
                    errors.append(f'{path}: schema alternative removed')
                else:
                    for index, member in enumerate(left):
                        compare(member, right[index], f'{path}/{index}', active_refs)
            elif left != right:
                errors.append(f'{path}: schema alternatives/security changed')
        elif left != right:
            errors.append(f'{path}: {left!r} changed to {right!r}')
    for section in ('paths', 'components', 'security'):
        compare(old.get(section, {}), new.get(section, {}), section)
    return errors

def validate_schema(doc, schema, value, path='body', depth=0):
    if depth > 64:
        return [f'{path}: schema recursion exceeds 64 levels']
    schema = resolve(doc, schema)
    errors = []
    for keyword in ('oneOf', 'anyOf'):
        if keyword in schema:
            matches = [not validate_schema(doc, child, value, path, depth + 1) for child in schema[keyword]]
            return [] if (sum(matches) == 1 if keyword == 'oneOf' else any(matches)) else [f'{path}: {keyword} mismatch']
    for child in schema.get('allOf', []):
        errors.extend(validate_schema(doc, child, value, path, depth + 1))
    kind = schema.get('type')
    if isinstance(kind, list):
        if value is None and 'null' in kind:
            return errors
        kind = next((t for t in kind if t != 'null'), None)
    types = {'object': dict, 'array': list, 'string': str, 'integer': int, 'number': (int, float), 'boolean': bool, 'null': type(None)}
    if kind in types and (not isinstance(value, types[kind]) or kind in {'integer', 'number'} and isinstance(value, bool)):
        return [f'{path}: expected {kind}']
    if 'enum' in schema and value not in schema['enum']:
        errors.append(f'{path}: unknown enum value')
    if isinstance(value, dict):
        for name in schema.get('required', []):
            if name not in value:
                errors.append(f'{path}/{name}: required field missing')
        for name, child in schema.get('properties', {}).items():
            if name in value:
                errors.extend(validate_schema(doc, child, value[name], f'{path}/{name}', depth + 1))
        for name, child in value.items():
            if isinstance(schema.get('propertyNames'), dict):
                errors.extend(validate_schema(doc, schema['propertyNames'], name, f'{path}/{name} (property name)', depth + 1))
            if name not in schema.get('properties', {}) and isinstance(schema.get('additionalProperties'), dict):
                errors.extend(validate_schema(doc, schema['additionalProperties'], child, f'{path}/{name}', depth + 1))
        if schema.get('additionalProperties') is False and value.keys() - schema.get('properties', {}).keys():
            errors.append(f'{path}: unknown properties')
    if isinstance(value, list):
        for index, child in enumerate(value):
            errors.extend(validate_schema(doc, schema.get('items', {}), child, f'{path}/{index}', depth + 1))
    if isinstance(value, str) and schema.get('pattern') and not re.search(schema['pattern'], value):
        errors.append(f'{path}: pattern mismatch')
    if isinstance(value, str) and schema.get('format') in {'date-time', 'date'}:
        try:
            if schema['format'] == 'date-time':
                parsed = datetime.datetime.fromisoformat(value.replace('Z', '+00:00'))
                if parsed.tzinfo is None or 'T' not in value:
                    raise ValueError('timezone required')
            else:
                datetime.date.fromisoformat(value)
        except ValueError:
            errors.append(f'{path}: invalid {schema["format"]}')
    return errors

def validate_replay(doc, case):
    headers = {k.lower(): v for k, v in case.get('headers', {}).items()}
    errors = []
    if case.get('generic_problem'):
        response = {'content': {'application/problem+json': {'schema': {'$ref': '#/components/schemas/ProblemDetail'}}}}
        if case.get('body', {}).get('status') != case['status']:
            errors.append('problem status must equal HTTP status')
    else:
        operation = doc.get('paths', {}).get(case['path'], {}).get(case['method'].lower())
        if operation is None:
            return [f'{case["path"]}: missing operation']
        response = operation.get('responses', {}).get(str(case['status']))
        if response is None:
            return [f'{case["path"]}: undocumented status {case["status"]}']
        response = resolve(doc, response)
    for name in response.get('headers', {}):
        if name.lower() not in headers:
            errors.append(f'{case["path"]}: missing header {name}')
    content = response.get('content', {})
    if content:
        media_type = headers.get('content-type', '').split(';')[0]
        if media_type not in content:
            errors.append(f'{case["path"]}: incompatible content-type {media_type}')
        else:
            errors.extend(validate_schema(doc, content[media_type].get('schema', {}), case.get('body')))
    for name, value in case.get('expected_headers', {}).items():
        if headers.get(name.lower()) != value:
            errors.append(f'{case["path"]}: changed header {name}')
    return errors

def replay_coverage(cases):
    if not any(case.get('path') == '/api/v1/tenant/usage' and case.get('method', '').lower() == 'get'
               and case.get('status') == 200 for case in cases):
        return ['replay requires an observed successful tenant usage journey']
    return []

def sample(doc, schema, locale, name='', depth=0):
    if depth > 12:
        return None
    schema = resolve(doc, schema)
    if isinstance(schema.get('type'), list):
        schema = dict(schema, type=next((t for t in schema['type'] if t != 'null'), 'null'))
    if 'allOf' in schema:
        values = [sample(doc, child, locale, name, depth + 1) for child in schema['allOf']]
        return {key: value for item in values if isinstance(item, dict) for key, value in item.items()}
    if name.endswith('currency') or name == 'currency_code':
        return 'JPY' if locale == 'ja-JP' else 'USD'
    if (name.endswith('_minor') or name in {'minor', 'minor_units', 'amount', 'unit_amount', 'subtotal'}) and schema.get('type') == 'integer':
        return 1200 if locale == 'ja-JP' else 1299
    if name in {'locale', 'default_locale'}:
        return locale.split('-')[0]
    if schema.get('format') == 'snowflake-id':
        return '873698342314721281'
    if schema.get('format') == 'date-time':
        return '2026-10-09T00:00:00Z'
    if schema.get('format') == 'date':
        return '2026-10-09'
    if schema.get('format') == 'uuid':
        return '019a1ff2-c333-7a00-8123-123456789abc'
    if 'example' in schema:
        return schema['example']
    if schema.get('enum'):
        return schema['enum'][0]
    if 'properties' in schema:
        return {key: sample(doc, child, locale, key, depth + 1) for key, child in schema['properties'].items()}
    if isinstance(schema.get('additionalProperties'), dict):
        keys = schema.get('propertyNames', {}).get('enum', [])
        key = keys[0] if keys else ('JPY' if locale == 'ja-JP' else 'USD') if name.endswith('_minor') else 'example'
        return {key: sample(doc, schema['additionalProperties'], locale, name, depth + 1)}
    if schema.get('type') == 'array':
        return [sample(doc, schema.get('items', {}), locale, name, depth + 1)]
    if 'oneOf' in schema or 'anyOf' in schema:
        return sample(doc, (schema.get('oneOf') or schema['anyOf'])[0], locale, name, depth + 1)
    return {'integer': 1, 'number': 1, 'boolean': True, 'string': 'example', 'null': None}.get(schema.get('type'), {})

def documentation(context, doc):
    lines = [f'# {context.replace("_", " ").title()} API', '', 'Generated by `scripts/api-docs.sh`; edit Rust annotations and regenerate.', '',
             'Examples: en-US / USD and ja-JP / JPY. Integer minor units: USD 1299 = $12.99; JPY 1200 = ¥1,200 (zero decimals).', '']
    for path, item in doc['paths'].items():
        for method, operation in item.items():
            if method not in METHODS:
                continue
            lines.extend([f'## {method.upper()} {path}', '', f'Operation: `{operation["operationId"]}`', '', operation.get('summary', ''), '',
                          'Responses: ' + ', '.join(str(s) for s in operation.get('responses', {})), ''])
            parameters = item.get('parameters', []) + operation.get('parameters', [])
            if parameters:
                lines.extend(['| Parameter | Location | Required | Type / bounds / default |', '| --- | --- | --- | --- |'])
                for parameter in parameters:
                    parameter = resolve(doc, parameter)
                    schema = parameter.get('schema', {})
                    details = ', '.join(f'{key}={json.dumps(schema[key], ensure_ascii=False)}' for key in ('type', 'format', 'minimum', 'maximum', 'default') if key in schema)
                    lines.append(f'| `{parameter["name"]}` | {parameter["in"]} | {"yes" if parameter.get("required") else "no"} | {details} |')
                lines.append('')
            request = operation.get('requestBody', {}).get('content', {}).get('application/json', {}).get('schema')
            if request:
                for locale in ('en-US', 'ja-JP'):
                    body = sample(doc, request, locale)
                    issues = validate_schema(doc, request, body)
                    if issues:
                        raise ValueError(f'{path} generated request example: ' + ', '.join(issues))
                    required_headers = [f'{p["name"]}: {sample(doc, p.get("schema", {}), locale)}' for p in operation.get('parameters', []) if p.get('in') == 'header' and p.get('required')]
                    money_locale = locale + (' / JPY, zero-decimal yen' if locale == 'ja-JP' else ' / USD, cents')
                    lines.extend([f'### Request journey ({money_locale})', '', '```http', f'{method.upper()} {path}', f'Accept-Language: {locale}',
                                  'Content-Type: application/json', *required_headers, '', json.dumps(body, ensure_ascii=False, indent=2), '```', ''])
            for status, response in operation.get('responses', {}).items():
                if str(status).startswith('2'):
                    for media in response.get('content', {}).values():
                        if 'schema' in media:
                            for locale in ('en-US', 'ja-JP'):
                                body = sample(doc, media['schema'], locale)
                                issues = validate_schema(doc, media['schema'], body)
                                if issues:
                                    raise ValueError(f'{path} generated response example: ' + ', '.join(issues))
                                money_locale = locale + (' / JPY, zero-decimal yen' if locale == 'ja-JP' else ' / USD, cents')
                                lines.extend([f'### Response {status} ({money_locale})', '', '```json', json.dumps(body, ensure_ascii=False, indent=2), '```', ''])
    for name, schema in doc.get('components', {}).get('schemas', {}).items():
        if any(re.search(r'(amount|price|total|balance).*(_minor|amount)', path) for path, _ in walk(schema)):
            lines.extend([f'## Money representation: {name}', ''])
            for locale in ('en-US', 'ja-JP'):
                lines.extend([f'### {locale}', '', '```json', json.dumps(sample(doc, schema, locale), ensure_ascii=False, indent=2), '```', ''])
    return '\n'.join(lines)

def shared_contract(documents):
    """Registry extracted from the runtime's type_slug mappings, not a second ledger."""
    slugs = set()
    for source in [ROOT / 'crates/platform/http/src/error.rs', *sorted((ROOT / 'crates/contexts').glob('*/src/domain/error.rs'))]:
        if not source.exists():
            continue
        text = source.read_text(encoding='utf-8')
        for start in re.finditer(r'fn type_slug\b[^\{]*\{', text):
            depth, end = 1, start.end()
            while depth and end < len(text):
                depth += (text[end] == '{') - (text[end] == '}')
                end += 1
            slugs.update(re.findall(r'"((?:[a-z][a-z0-9-]*/)?[a-z][a-z0-9-]*)"', text[start.end():end]))
    advertising = ROOT / 'crates/contexts/advertising/src/adapters/http/handlers.rs'
    if advertising.exists():
        slugs.update(re.findall(r'\#\[serde\(rename = "(advertising/[a-z-]+)"\)\]', advertising.read_text(encoding='utf-8')))
    problem = next((doc.get('components', {}).get('schemas', {}).get('ProblemDetail') for _, doc in documents
                    if doc.get('components', {}).get('schemas', {}).get('ProblemDetail')), None)
    if problem is None:
        raise ValueError('generated contracts are missing ProblemDetail')
    return {'openapi': '3.1.0', 'info': {'title': 'Shared v1 wire conventions', 'version': '1.0.0'}, 'paths': {},
            'components': {'schemas': {'ProblemDetail': problem,
                'ProblemType': {'type': 'string', 'format': 'uri', 'enum': ['https://sanvi.dev/problems/' + s for s in sorted(slugs)]}},
                'parameters': {'After': {'name': 'after', 'in': 'query', 'required': False, 'schema': {'type': 'string'},
                    'description': 'Opaque cursor from the previous page, passed unchanged.'},
                    'Limit': {'name': 'limit', 'in': 'query', 'required': False, 'schema': {'type': 'integer', 'minimum': 1},
                        'description': 'Bounded page size; each operation declares its resource-specific maximum and default.'}}}}

def freeze(base_dir, current_dir, base_ref=None):
    errors = []
    if base_ref:
        files = subprocess.check_output(['git', 'ls-tree', '-r', '--name-only', base_ref, 'api/.frozen'], cwd=ROOT, text=True).splitlines()
        if files:
            changed = subprocess.check_output(['git', 'diff', '--name-only', base_ref, '--', 'api/.frozen'], cwd=ROOT, text=True).splitlines()
            if changed:
                errors.append('frozen baseline is immutable; changed: ' + ', '.join(changed))
            # The immutable launch snapshot remains an anchor. Once frozen,
            # every accepted addition also becomes part of the public API;
            # compare against the PR base so later removal cannot erase it.
            public_files = subprocess.check_output(['git', 'ls-tree', '-r', '--name-only', base_ref, 'api/'], cwd=ROOT, text=True).splitlines()
            for filename in public_files:
                path = pathlib.PurePosixPath(filename)
                if path.parent != pathlib.PurePosixPath('api') or path.suffix != '.yaml':
                    continue
                current = current_dir / path.name
                if not current.exists():
                    errors.append(f'{path.name}: spec removed from PR base')
                else:
                    previous = yaml.safe_load(subprocess.check_output(['git', 'show', f'{base_ref}:{filename}'], cwd=ROOT, text=True))
                    errors.extend(f'{path.name} (PR base): {error}' for error in breaking(previous, load(current)))
    baseline = sorted(base_dir.rglob('*.yaml'))
    if not baseline:
        return errors + ['missing frozen baseline']
    for path in baseline:
        relative = path.relative_to(base_dir)
        current = current_dir / relative
        if not current.exists():
            errors.append(f'{relative}: spec removed')
        else:
            errors.extend(f'{relative}: {e}' for e in breaking(load(path), load(current)))
    manifest = base_dir / 'manifest.json'
    if not manifest.exists():
        errors.append('missing frozen manifest')
    else:
        actual = {p.relative_to(base_dir).as_posix(): hashlib.sha256(p.read_bytes()).hexdigest() for p in baseline}
        if actual != json.loads(manifest.read_text(encoding='utf-8')):
            errors.append('frozen snapshot integrity failure')
    return errors

def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('command', choices=['lint', 'freeze', 'docs', 'replay'])
    parser.add_argument('--api-dir', type=pathlib.Path, default=ROOT / 'api')
    parser.add_argument('--baseline', type=pathlib.Path, default=ROOT / 'api/.frozen')
    parser.add_argument('--base-ref')
    parser.add_argument('--allow-breaking', action='store_true', help='CI-reviewed exception; never permits baseline edits')
    parser.add_argument('--check', action='store_true')
    parser.add_argument('--capture', type=pathlib.Path)
    args = parser.parse_args()
    errors = []
    documents = [(path, load(path)) for path in sorted(args.api_dir.glob('*.yaml'))]
    if not documents:
        errors.append('no OpenAPI documents found')
    if args.command == 'lint':
        errors.extend(schema_name_collisions((path.relative_to(ROOT), path.read_text(encoding='utf-8'))
            for path in (ROOT / 'crates').rglob('*.rs')))
        operations = set()
        schemas = {}
        for path, doc in documents:
            errors.extend(f'{path.name}: {e}' for e in lint(doc))
            for name, schema in doc.get('components', {}).get('schemas', {}).items():
                if name in schemas and schemas[name] != schema:
                    errors.append(f'{path.name}: conflicting shared schema {name}')
                schemas[name] = schema
            for _, value in walk(doc.get('paths', {})):
                if isinstance(value, dict) and 'operationId' in value:
                    operation = value['operationId']
                    if operation in operations:
                        errors.append(f'{path.name}: duplicate operationId {operation}')
                    operations.add(operation)
    elif args.command == 'freeze':
        findings = freeze(args.baseline, args.api_dir, args.base_ref)
        if args.allow_breaking:
            errors.extend(e for e in findings if e.startswith(('frozen ', 'missing ')))
            if findings:
                print('Reviewed API exception: ' + '\n'.join(findings))
        else:
            errors.extend(findings)
    elif args.command == 'docs':
        shared = args.api_dir / 'shared/common.yaml'
        content = yaml.safe_dump(shared_contract(documents), sort_keys=True, allow_unicode=True)
        if args.check:
            if not shared.exists() or shared.read_text(encoding='utf-8') != content:
                errors.append('shared schema/problem registry drift')
        else:
            shared.parent.mkdir(parents=True, exist_ok=True)
            shared.write_text(content, encoding='utf-8', newline='\n')
        output = ROOT / 'docs/api/generated'
        output.mkdir(parents=True, exist_ok=True)
        index = '\n'.join(['# Public API v1', '', '[Style guide](style-guide.md) · [Deprecation policy](deprecation-policy.md) · [Deprecation changelog](deprecations.md)', '',
            'Generated from the Rust OpenAPI contract. Journey examples show en-US/USD and ja-JP/JPY; amounts are integer minor units and JPY has zero decimals.', '',
            *[f'- [{path.stem.replace("_", " ").title()}](generated/{path.stem}.md)' for path, _ in documents], '',
            'Build: `just api-docs`. Verify: `just api-docs --check`. Replay candidate responses: `just replay-suite rc.8`.', ''])
        index_path = output.parent / 'README.md'
        if args.check:
            if not index_path.exists() or index_path.read_text(encoding='utf-8') != index:
                errors.append('generated public documentation index drift')
        else:
            index_path.write_text(index, encoding='utf-8', newline='\n')
        for path, doc in documents:
            target, text = output / (path.stem + '.md'), documentation(path.stem, doc)
            if args.check:
                if not target.exists() or target.read_text(encoding='utf-8') != text:
                    errors.append(f'{target}: generated docs drift')
            else:
                target.write_text(text, encoding='utf-8', newline='\n')
    elif args.command == 'replay':
        if args.capture is None or not args.capture.exists():
            errors.append('replay requires real Rust response capture (--capture)')
        else:
            merged = {'paths': {}, 'components': {'schemas': {}}}
            for _, doc in documents:
                merged['paths'].update(doc['paths'])
                merged['components']['schemas'].update(doc.get('components', {}).get('schemas', {}))
            cases = json.loads(args.capture.read_text(encoding='utf-8'))
            usage = pathlib.Path(str(args.capture) + '.usage.json')
            if usage.exists():
                cases.extend(json.loads(usage.read_text(encoding='utf-8')))
            if not cases:
                errors.append('empty replay capture')
            errors.extend(replay_coverage(cases))
            for case in cases:
                errors.extend(validate_replay(merged, case))
    if errors:
        print('\n'.join(errors), file=sys.stderr)
        return 1
    print(f'api-contract {args.command}: OK ({len(documents)} documents)')
    return 0

if __name__ == '__main__':
    sys.exit(main())
