"""Mutation proofs for public contract gates."""
import copy
import importlib.util
import pathlib
import tempfile
import json
import hashlib
import subprocess
import sys
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location('contract', pathlib.Path(__file__).with_name('api-contract.py'))
contract = importlib.util.module_from_spec(spec)
spec.loader.exec_module(contract)

def document():
    return {'openapi': '3.1.0', 'paths': {'/api/v1/things': {'get': {
        'operationId': 'list_things', 'tags': ['things'], 'parameters': [],
        'responses': {'200': {'description': 'Things', 'content': {'application/json': {'schema': {
            'type': 'object', 'required': ['id', 'state'], 'properties': {
                'id': {'type': 'string'}, 'state': {'type': 'string', 'enum': ['active', 'paused']}}}}}}}}}}}

class ContractTests(unittest.TestCase):
    def test_freeze_preserves_optional_fields_added_after_initial_snapshot(self):
        with tempfile.TemporaryDirectory() as directory:
            root = pathlib.Path(directory)
            baseline, current = root / 'frozen', root / 'current'
            baseline.mkdir()
            current.mkdir()
            initial = document()
            text = json.dumps(initial)
            (baseline / 'test.yaml').write_text(text, encoding='utf-8')
            (baseline / 'manifest.json').write_text(json.dumps({'test.yaml': hashlib.sha256(text.encode()).hexdigest()}), encoding='utf-8')
            (current / 'test.yaml').write_text(text, encoding='utf-8')
            base = copy.deepcopy(initial)
            base['paths']['/api/v1/things']['get']['responses']['200']['content']['application/json']['schema']['properties']['note'] = {'type': 'string'}
            def git_output(command, **kwargs):
                if command[1] == 'diff':
                    return ''
                if command[1] == 'show':
                    return json.dumps(base)
                if command[-1] == 'api/.frozen':
                    return 'api/.frozen/test.yaml\n'
                return 'api/test.yaml\n'
            with patch.object(contract.subprocess, 'check_output', side_effect=git_output):
                self.assertTrue(any('note' in finding for finding in contract.freeze(baseline, current, 'base')))
            # Introducing the freeze must not freeze the pre-consistency-pass API.
            def before_freeze(command, **kwargs):
                return '' if command[-1] == 'api/.frozen' else git_output(command, **kwargs)
            with patch.object(contract.subprocess, 'check_output', side_effect=before_freeze):
                self.assertEqual(contract.freeze(baseline, current, 'base'), [])
            additive = copy.deepcopy(base)
            additive['paths']['/api/v1/things']['get']['responses']['200']['content']['application/json']['schema']['properties']['another_note'] = {'type': 'string'}
            (current / 'test.yaml').write_text(json.dumps(additive), encoding='utf-8')
            with patch.object(contract.subprocess, 'check_output', side_effect=git_output):
                self.assertEqual(contract.freeze(baseline, current, 'base'), [])

    def test_real_tenant_usage_map_values_and_names_are_validated(self):
        doc = contract.load(contract.ROOT / 'api/tenancy.yaml')
        case = {'path': '/api/v1/tenant/usage', 'method': 'get', 'status': 200,
                'headers': {'content-type': 'application/json'},
                'body': {'plan': 'starter', 'period_start': '2026-10-01T00:00:00Z',
                         'period_end': '2026-11-01T00:00:00Z',
                         'dimensions': {'api_requests': {'used': 1, 'limit': 7}}}}
        self.assertEqual(contract.validate_replay(doc, case), [])
        for mutation in [lambda d: d['api_requests'].update(used='broken'),
                         lambda d: d['api_requests'].pop('limit'),
                         lambda d: d.update(unknown_dimension={'used': 1, 'limit': 7})]:
            broken = copy.deepcopy(case)
            mutation(broken['body']['dimensions'])
            self.assertTrue(contract.validate_replay(doc, broken))

    def test_repeated_reference_siblings_are_checked_at_each_use(self):
        old = {'components': {'schemas': {'Label': {'type': 'string'}, 'Pair': {'type': 'object', 'properties': {
            'first': {'$ref': '#/components/schemas/Label'}, 'second': {'$ref': '#/components/schemas/Label'}}}}}}
        new = copy.deepcopy(old)
        new['components']['schemas']['Pair']['properties']['second']['enum'] = ['restricted']
        self.assertTrue(contract.breaking(old, new))

    def test_replay_requires_observed_tenant_journey(self):
        self.assertTrue(contract.replay_coverage([{'path': '/healthz', 'method': 'get', 'status': 200}]))
        self.assertEqual(contract.replay_coverage([{'path': '/api/v1/tenant/usage', 'method': 'get', 'status': 200}]), [])

    def test_source_schema_names_require_explicit_namespaces(self):
        source = '#[derive(ToSchema)]\npub struct Money { amount: i64 }'
        self.assertTrue(contract.schema_name_collisions([('a.rs', source), ('b.rs', source)]))
        alias = '#[derive(ToSchema)]\n#[schema(as = OtherMoney)]\npub struct Money { minor: i64 }'
        self.assertEqual(contract.schema_name_collisions([('a.rs', source), ('b.rs', alias)]), [])

    def test_command_line_freeze_negative_and_additive_proofs(self):
        with tempfile.TemporaryDirectory() as directory:
            root = pathlib.Path(directory)
            baseline, current = root / 'frozen', root / 'current'
            baseline.mkdir()
            current.mkdir()
            old = document()
            text = json.dumps(old)
            (baseline / 'test.yaml').write_text(text, encoding='utf-8')
            (baseline / 'manifest.json').write_text(json.dumps({'test.yaml': hashlib.sha256(text.encode()).hexdigest()}), encoding='utf-8')
            schema = lambda d: d['paths']['/api/v1/things']['get']['responses']['200']['content']['application/json']['schema']
            changes = [(lambda s: s['properties'].pop('id'), 1),
                       (lambda s: s['properties']['state'].update(enum=['active']), 1),
                       (lambda s: s.update(required=['id']), 1),
                       (lambda s: s['properties'].update(note={'type': 'string'}), 0)]
            for mutate, expected in changes:
                changed = copy.deepcopy(old)
                mutate(schema(changed))
                (current / 'test.yaml').write_text(json.dumps(changed), encoding='utf-8')
                result = subprocess.run([sys.executable, str(pathlib.Path(__file__).with_name('api-contract.py')), 'freeze',
                    '--baseline', str(baseline), '--api-dir', str(current)], capture_output=True, text=True)
                self.assertEqual(result.returncode, expected, result.stdout + result.stderr)

    def test_lint_bad_endpoint_and_dangling_ref(self):
        doc = document()
        self.assertEqual(contract.lint(doc), [])
        doc['paths']['/api/v1/Bad_Things'] = doc['paths'].pop('/api/v1/things')
        self.assertTrue(contract.lint(doc))
        doc = document()
        doc['components'] = {'schemas': {'Bad': {'$ref': '#/components/schemas/Missing'}}}
        self.assertTrue(contract.lint(doc))

    def test_breaking_mutations_and_additive_change(self):
        schema = lambda d: d['paths']['/api/v1/things']['get']['responses']['200']['content']['application/json']['schema']
        for mutation in [lambda s: s['properties'].pop('id'), lambda s: s['properties']['state'].update(enum=['active']),
                         lambda s: s.update(required=['id']), lambda s: s['properties']['id'].update(type='integer')]:
            old = document()
            new = copy.deepcopy(old)
            mutation(schema(new))
            self.assertTrue(contract.breaking(old, new))
        old = document()
        new = copy.deepcopy(old)
        schema(new)['properties']['note'] = {'type': 'string'}
        self.assertEqual(contract.breaking(old, new), [])

    def test_required_additive_response_fields_preserve_request_contracts(self):
        old = document()
        operation = old['paths']['/api/v1/things']['get']
        schema = operation['responses']['200']['content']['application/json']['schema']
        old['components'] = {'schemas': {'Thing': copy.deepcopy(schema)}}
        operation['responses']['200']['content']['application/json']['schema'] = {'$ref': '#/components/schemas/Thing'}
        new = copy.deepcopy(old)
        added = new['components']['schemas']['Thing']
        added['properties']['new_guarantee'] = {'type': 'string'}
        added.setdefault('required', []).append('new_guarantee')
        self.assertEqual(contract.breaking(old, new), [])
        for doc in (old, new):
            doc['paths']['/api/v1/things']['post'] = {
                'requestBody': {'content': {'application/json': {'schema': {'$ref': '#/components/schemas/Thing'}}}},
                'responses': {'204': {'description': 'Created'}},
            }
        self.assertTrue(contract.breaking(old, new))
        for doc in (old, new):
            doc['components']['requestBodies'] = {'Input': doc['paths']['/api/v1/things']['post']['requestBody']}
            doc['paths']['/api/v1/things']['post']['requestBody'] = {'$ref': '#/components/requestBodies/Input'}
        self.assertTrue(contract.breaking(old, new))

    def test_existing_optional_response_field_cannot_become_required(self):
        old = document()
        schema = old['paths']['/api/v1/things']['get']['responses']['200']['content']['application/json']['schema']
        schema['properties']['note'] = {'type': 'string'}
        new = copy.deepcopy(old)
        new['paths']['/api/v1/things']['get']['responses']['200']['content']['application/json']['schema']['required'].append('note')
        self.assertTrue(contract.breaking(old, new))

    def test_parameters_status_content_security(self):
        for mutation in [lambda o: o['responses'].pop('200'), lambda o: o['responses']['200'].update(content={}),
                         lambda o: o.update(parameters=[{'name': 'filter', 'in': 'query', 'required': True}]),
                         lambda o: o.update(security=[{'bearer': []}])]:
            old = document()
            new = copy.deepcopy(old)
            mutation(new['paths']['/api/v1/things']['get'])
            self.assertTrue(contract.breaking(old, new))

    def test_replay_shape_headers_status(self):
        doc = document()
        case = {'path': '/api/v1/things', 'method': 'get', 'status': 200,
                'headers': {'content-type': 'application/json'}, 'body': {'id': '123', 'state': 'active'}}
        self.assertEqual(contract.validate_replay(doc, case), [])
        for mutation in [lambda c: c['body'].pop('id'), lambda c: c.update(status=201),
                         lambda c: c['headers'].update({'content-type': 'text/plain'})]:
            new = copy.deepcopy(case)
            mutation(new)
            self.assertTrue(contract.validate_replay(doc, new))

    def test_money_docs_locales(self):
        doc = document()
        doc['components'] = {'schemas': {'Checkout': {'type': 'object', 'properties': {'amount_minor': {'type': 'integer'}, 'currency': {'type': 'string'}}}}}
        text = contract.documentation('payments', doc)
        for value in ['JPY', 'ja-JP', 'en-US', '1200', 'Checkout']:
            self.assertIn(value, text)

    def test_frozen_snapshot_integrity(self):
        with tempfile.TemporaryDirectory() as directory:
            root = pathlib.Path(directory)
            baseline, current = root / 'frozen', root / 'current'
            baseline.mkdir()
            current.mkdir()
            text = json.dumps(document())
            (baseline / 'test.yaml').write_text(text, encoding='utf-8')
            (current / 'test.yaml').write_text(text, encoding='utf-8')
            (baseline / 'manifest.json').write_text(json.dumps({'test.yaml': hashlib.sha256(text.encode()).hexdigest()}), encoding='utf-8')
            self.assertEqual(contract.freeze(baseline, current), [])
            (baseline / 'test.yaml').write_text(text + '\n', encoding='utf-8')
            self.assertIn('frozen snapshot integrity failure', contract.freeze(baseline, current))

    def test_referenced_schema_and_new_restrictions(self):
        old = document()
        old['components'] = {'schemas': {'Thing': {'type': 'string'}}}
        new = copy.deepcopy(old)
        new['components']['schemas']['Thing']['enum'] = ['limited']
        self.assertTrue(contract.breaking(old, new))

    def test_money_map_and_boolean_are_not_scalar_amounts(self):
        schema = {'type': 'object', 'additionalProperties': {'type': 'integer'}}
        self.assertEqual(contract.sample({}, schema, 'ja-JP', 'minimum_budgets_minor'), {'JPY': 1200})
        self.assertIs(contract.sample({}, {'type': 'boolean'}, 'ja-JP', 'is_minor'), True)

    def test_required_parameter_reference_is_breaking(self):
        old = document()
        new = copy.deepcopy(old)
        new['components'] = {'parameters': {'Filter': {'name': 'filter', 'in': 'query', 'required': True, 'schema': {'type': 'string'}}}}
        new['paths']['/api/v1/things']['get']['parameters'] = [{'$ref': '#/components/parameters/Filter'}]
        self.assertTrue(contract.breaking(old, new))

    def test_properties_named_like_metadata_are_frozen(self):
        old = document()
        schema = old['paths']['/api/v1/things']['get']['responses']['200']['content']['application/json']['schema']
        schema['properties']['title'] = {'type': 'string'}
        new = copy.deepcopy(old)
        new['paths']['/api/v1/things']['get']['responses']['200']['content']['application/json']['schema']['properties'].pop('title')
        self.assertTrue(contract.breaking(old, new))

if __name__ == '__main__':
    unittest.main()
