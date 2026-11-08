"""The released design source cannot be substituted behind matching outer metadata."""
import json
from pathlib import Path
import shutil
import tempfile
import unittest

import design_system


class DesignSourcePin(unittest.TestCase):
    def test_modified_extra_or_symlinked_sources_cannot_pass_the_released_pin(self):
        source = Path(__file__).resolve().parent.parent
        with tempfile.TemporaryDirectory(prefix='nddev-design-source-') as temporary:
            root = Path(temporary)
            (root / 'packaging').mkdir()
            shutil.copyfile(source / 'packaging/design-system.json', root / 'packaging/design-system.json')
            package = root / 'vendor/nddev-opennetwork-design-tokens'
            shutil.copytree(source / 'vendor/nddev-opennetwork-design-tokens', package)
            pin = json.loads((root / 'packaging/design-system.json').read_text())
            self.assertEqual(design_system.verify(root)['commit'], pin['commit'])
            invalid = json.loads(json.dumps(pin))
            invalid['files'].append(invalid['files'][0])
            (root / 'packaging/design-system.json').write_text(json.dumps(invalid))
            with self.assertRaisesRegex(ValueError, 'inventory or bytes'):
                design_system.verify(root)
            invalid = json.loads(json.dumps(pin))
            invalid['files'][0]['sha256'] = None
            (root / 'packaging/design-system.json').write_text(json.dumps(invalid))
            with self.assertRaisesRegex(ValueError, 'file records'):
                design_system.verify(root)
            (root / 'packaging/design-system.json').write_text(json.dumps(pin))
            selected = package / 'fonts/IBMPlexSans-Regular.ttf'
            original = selected.read_bytes()
            selected.write_bytes(original + b'substituted')
            with self.assertRaisesRegex(ValueError, 'inventory or bytes'):
                design_system.verify(root)
            selected.write_bytes(original)
            extra = package / 'unexpected'
            extra.write_text('unreleased source')
            with self.assertRaisesRegex(ValueError, 'inventory or bytes'):
                design_system.verify(root)
            extra.unlink()
            target = root / 'outside-font'
            selected.rename(target)
            selected.symlink_to(target)
            with self.assertRaisesRegex(ValueError, 'nonregular'):
                design_system.verify(root)
