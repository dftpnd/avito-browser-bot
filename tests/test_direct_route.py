"""Pure configuration tests: no root, no real config reads or network changes."""
import importlib.util
from pathlib import Path
import unittest

spec = importlib.util.spec_from_file_location('avito_direct', Path(__file__).parents[1] / 'scripts/configure_avito_direct.py')
module = importlib.util.module_from_spec(spec)
spec.loader.exec_module(module)


class DirectRouteTest(unittest.TestCase):
    def config(self):
        return {
            'inbounds': [{'protocol': 'dokodemo-door', 'settings': {'followRedirect': True}, 'sniffing': {
                'enabled': True, 'destOverride': ['http', 'tls', 'quic']}}],
            'outbounds': [{'tag': 'existing-vpn', 'protocol': 'vless', 'settings': {'unchanged': True}}],
            'routing': {'rules': [{'type': 'field', 'domain': ['example.org'], 'outboundTag': 'existing-vpn'}]},
        }

    def test_only_avito_rule_and_direct_outbound_are_added(self):
        original = self.config()
        result = module.prepare(original)
        self.assertEqual(result['routing']['rules'][1:], original['routing']['rules'])
        self.assertEqual(result['outbounds'][:-1], original['outbounds'])
        self.assertEqual(result['inbounds'], original['inbounds'])
        self.assertNotIn(module.TAG, str(original))
        module.expected(result)

    def test_repeated_apply_is_idempotent(self):
        once = module.prepare(self.config())
        self.assertEqual(once, module.prepare(once))

    def test_unknown_routing_is_rejected(self):
        for invalid in [
            {**self.config(), 'inbounds': []},
            {**self.config(), 'routing': {'script': 'custom'}},
            {**self.config(), 'outbounds': [{'tag': module.TAG, 'protocol': 'socks'}]},
        ]:
            with self.assertRaises(RuntimeError):
                module.prepare(invalid)

    def test_fixed_dns_inbound_does_not_require_browser_sniffing(self):
        config = self.config()
        config['inbounds'].append({'protocol': 'dokodemo-door', 'settings': {'address': '1.1.1.1', 'port': 53}})
        module.expected(module.prepare(config))

    def test_direct_redirect_is_not_accepted(self):
        config = module.prepare(self.config())
        config['outbounds'][-1]['settings'] = {'redirect': '127.0.0.1:1080'}
        with self.assertRaises(RuntimeError):
            module.expected(config)


if __name__ == '__main__':
    unittest.main()
