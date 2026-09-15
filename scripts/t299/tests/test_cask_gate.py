import ast
import hashlib
import io
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import cask_gate as gate


class CandidateTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix='memory-cask-test-')
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.tap = self.root / 'tap'
        self.tap.mkdir()
        self.calls = []
        self.fail = None
        self.push = 'success'
        self.search_result = gate.QUALIFIED + '\n'
        self.queue_busy = False
        self.remote = None
        self.repo = self.root / 'brew'
        (self.repo / 'Library/Taps').mkdir(parents=True)
        self.cache = self.root / 'Memory_0.5.8_aarch64.dmg'
        self.cache.write_bytes(b'fixture dmg')
        self.sha = hashlib.sha256(self.cache.read_bytes()).hexdigest()
        self.config = self.root / 'audit.json'
        self.config.write_text(json.dumps({'exit_code': 1, 'stdout_lines': [], 'stderr_lines': ['fixture self-signed incompatibility']}))
        self.git('init', '-b', 'main')
        self.git('config', 'user.name', 'Fixture')
        self.git('config', 'user.email', 'fixture@example.invalid')
        (self.tap / 'README.md').write_text('fixture tap\n')
        self.git('add', '--', 'README.md')
        self.git('commit', '-m', 'baseline')
        self.baseline = self.git('rev-parse', 'HEAD').stdout.strip()
        self.git('update-ref', 'refs/remotes/origin/main', self.baseline)
        self.remote = self.baseline

    def git(self, *args):
        return subprocess.run(['git', *args], cwd=self.tap, capture_output=True, text=True, check=True)

    def runner(self, argv, **kwargs):
        self.calls.append(argv)
        result = lambda out='', err='', code=0: subprocess.CompletedProcess(argv, code, out, err)
        if argv[0] == 'gh':
            rows = [{'id': 999, 'status': 'queued'}] if self.queue_busy else []
            return result(json.dumps([{'total_count': len(rows), 'workflow_runs': rows}]))
        if argv[0] == 'git':
            if argv[1] == 'ls-remote':
                if self.remote == 'error':
                    raise OSError('fixture transport error')
                return result(self.remote + '\trefs/heads/main\n')
            if argv[1] == 'push':
                if self.push in ('accepted', 'accepted-nonzero', 'success'):
                    self.remote = self.git('rev-parse', 'HEAD').stdout.strip()
                elif self.push == 'unknown':
                    self.remote = 'error'
                elif self.push == 'diverged':
                    self.remote = 'b' * 40
                if self.push in ('accepted', 'unknown', 'unchanged-exception'):
                    raise OSError('fixture transport error')
                return result(code=0 if self.push == 'success' else 1)
            if self.fail == 'commit' and 'commit' in argv:
                return result(code=1)
            return subprocess.run(argv, **kwargs, capture_output=True, text=True)
        if self.fail and self.fail in argv:
            return result(err='unrelated failure', code=1)
        if argv == ['brew', '--repository']:
            return result(str(self.repo) + '\n')
        if argv[:2] == ['brew', 'info']:
            source = self.tap / gate.CASK
            cask = {'token': gate.TOKEN, 'full_token': gate.QUALIFIED, 'tap': 'webdesserts/tap',
                    'version': '0.5.8', 'sha256': self.sha, 'url': gate.asset_url('v0.5.8'),
                    'ruby_source_path': str(source)}
            if self.fail == 'resolution':
                cask['ruby_source_path'] = '/wrong/cask.rb'
            return result(json.dumps({'casks': [cask]}))
        if argv[:2] == ['brew', '--cache']:
            return result(str(self.cache) + '\n')
        if argv[:2] == ['brew', 'search']:
            return result('other/tap/webdesserts-memory\n' if self.fail == 'collision' else self.search_result)
        if argv[:2] == ['brew', 'audit']:
            return result(err='fixture self-signed incompatibility\n', code=1)
        if argv[0] in ('brew', 'ruby'):
            return result()
        self.failTest('unexpected command: ' + repr(argv))

    def validate(self, **kw):
        return gate.validate(self.tap, 'v0.5.8', self.sha, 'sonoma', self.config, runner=self.runner, **kw)

    def publish(self):
        return gate.publish(self.tap, 'v0.5.8', self.sha, 'sonoma', self.config, run_id='123', runner=self.runner)

    def assert_no_staging(self):
        self.assertEqual(self.git('diff', '--cached', '--name-only').stdout, '')
        self.assertEqual(self.git('rev-parse', 'HEAD').stdout.strip(), self.baseline)

    def test_generation_exact_and_required_inputs(self):
        expected = ('cask "webdesserts-memory" do\n'
                    '  version "0.5.8"\n'
                    '  sha256 "' + self.sha + '"\n\n'
                    '  url "https://github.com/webdesserts/obsidian-memory/releases/download/v0.5.8/Memory_0.5.8_aarch64.dmg"\n'
                    '  name "Memory"\n'
                    '  desc "Desktop companion for Obsidian memory"\n'
                    '  homepage "https://github.com/webdesserts/obsidian-memory"\n\n'
                    '  depends_on arch: :arm64\n'
                    '  depends_on macos: ">= :sonoma"\n\n'
                    '  app "Memory.app"\n\n'
                    '  caveats <<~EOS\n'
                    '    Memory is self-signed, not Apple-notarized. On first launch, macOS may block it.\n'
                    '    After verifying the release, approve it once in System Settings > Privacy & Security > Open Anyway.\n'
                    '    Never approve an unexpected warning. An upgrade may require approval again.\n'
                    '    The macOS floor is a reviewed support policy, not proof on every newer macOS release.\n'
                    '  EOS\n'
                    'end\n')
        self.assertEqual(gate.generate('v0.5.8', self.sha, 'sonoma'), expected)
        for tag, sha, floor in [('v0.5.8', self.sha, ''), ('v0.5.8', self.sha, 'invented'), ('v0.5.8', 'bad', 'sonoma'), ('latest', self.sha, 'sonoma')]:
            with self.subTest(tag=tag, sha=sha, floor=floor), self.assertRaises(gate.ValidationError):
                gate.generate(tag, sha, floor)
        for forbidden in ['no-quarantine', 'xattr', 'postflight', 'no_check', 'spctl']:
            self.assertNotIn(forbidden, expected)

    def test_default_validate_cannot_publish_and_repeat_uses_head(self):
        with patch.dict(os.environ, {}, clear=True), patch.object(gate, 'publish', side_effect=AssertionError('publication reached')):
            self.assertEqual(self.validate()['status'], 'validated')
            self.assertEqual(self.validate()['baseline'], self.baseline)
        self.assert_no_staging()
        self.assertFalse(any(a[0] == 'git' and a[1] in ('add', 'commit', 'push') for a in self.calls))
        self.assertFalse((self.repo / 'Library/Taps/webdesserts/homebrew-tap').exists())
        self.assertEqual(gate.parser().parse_args(['--tap', str(self.tap), '--tag', 'v0.5.8', '--sha256', self.sha, '--macos-floor', 'sonoma', '--audit-config', str(self.config)]).operation, 'validate')

    def test_default_cli_and_missing_floor_do_not_publish(self):
        arguments = ['cask_gate.py', '--tap', str(self.tap), '--tag', 'v0.5.8', '--sha256', self.sha,
                     '--macos-floor', 'sonoma', '--audit-config', str(self.config)]
        with patch.object(sys, 'argv', arguments), patch.object(gate, 'validate', return_value={'status': 'validated'}) as validate, patch.object(gate, 'publish', side_effect=AssertionError('publication reached')), patch('sys.stdout', new_callable=io.StringIO):
            self.assertEqual(gate.main(), 0)
            validate.assert_called_once()
        arguments.remove('--macos-floor')
        arguments.remove('sonoma')
        with patch.object(sys, 'argv', arguments), patch.object(gate, 'publish', side_effect=AssertionError('publication reached')), patch('sys.stderr', new_callable=io.StringIO):
            self.assertEqual(gate.main(), 1)
        self.assertFalse((self.tap / gate.CASK).exists())
        self.assert_no_staging()

    def test_gate_failures_never_stage_and_cleanup_mapping(self):
        for failure in ['resolution', 'fetch', 'style', 'audit', 'collision', 'search', 'ruby']:
            with self.subTest(failure=failure):
                self.fail = failure
                with self.assertRaises(gate.ValidationError):
                    self.validate()
                self.assert_no_staging()
                self.assertFalse((self.repo / 'Library/Taps/webdesserts/homebrew-tap').exists())
        self.fail = None
        self.cache.write_bytes(b'wrong digest')
        with self.assertRaises(gate.ValidationError):
            self.validate()
        self.assert_no_staging()

    def test_unrelated_dirty_staged_and_ref_mismatch_refuse(self):
        (self.tap / 'other').write_text('dirty')
        with self.assertRaises(gate.ValidationError):
            self.validate()
        (self.tap / 'other').unlink()
        self.git('update-ref', '-d', 'refs/remotes/origin/main')
        with self.assertRaises(gate.ValidationError):
            self.validate()
        self.assert_no_staging()

    def test_missing_audit_configuration_refuses_before_rewrite(self):
        self.config.unlink()
        with self.assertRaises(gate.ValidationError):
            self.validate()
        self.assertFalse((self.tap / gate.CASK).exists())
        self.assert_no_staging()

    def test_collision_availability_and_own_token_are_bounded(self):
        for output in ['', gate.QUALIFIED + '\n', gate.TOKEN + '\n']:
            self.search_result = output
            self.assertEqual(self.validate()['status'], 'validated')
        self.search_result = gate.QUALIFIED + '\nother/tap/' + gate.TOKEN
        with self.assertRaises(gate.ValidationError):
            self.validate()
        self.assert_no_staging()

    def test_published_head_is_baseline_for_repeat_validation(self):
        path = self.tap / gate.CASK
        path.parent.mkdir()
        path.write_text(gate.generate('v0.5.7', 'a' * 64, 'sonoma'))
        self.git('add', '--', gate.CASK)
        self.git('commit', '-m', 'older published cask')
        self.baseline = self.git('rev-parse', 'HEAD').stdout.strip()
        self.git('update-ref', 'refs/remotes/origin/main', self.baseline)
        self.assertEqual(self.validate()['baseline'], self.baseline)
        self.assertEqual(self.validate()['baseline'], self.baseline)
        self.assert_no_staging()
        path.write_text(path.read_text() + '# unrelated edit\n')
        with self.assertRaises(gate.ValidationError):
            self.validate()

    def test_queue_and_late_remote_changes_leave_no_staging(self):
        self.queue_busy = True
        with self.assertRaises(gate.ValidationError):
            self.publish()
        self.assertFalse((self.tap / gate.CASK).exists())
        self.assert_no_staging()
        self.queue_busy = False
        original = self.runner
        def changing(argv, **kwargs):
            result = original(argv, **kwargs)
            if argv[:2] == ['brew', 'audit']:
                self.remote = 'a' * 40
            return result
        with self.assertRaises(gate.ValidationError):
            gate.publish(self.tap, 'v0.5.8', self.sha, 'sonoma', self.config, run_id='123', runner=changing)
        self.assert_no_staging()

    def test_runner_exception_always_removes_owned_mapping(self):
        original = self.runner
        def raising(argv, **kwargs):
            if argv[:2] == ['brew', 'fetch']:
                raise OSError('fixture fetch failure')
            return original(argv, **kwargs)
        with self.assertRaises(OSError):
            gate.validate(self.tap, 'v0.5.8', self.sha, 'sonoma', self.config, runner=raising)
        self.assertFalse((self.repo / 'Library/Taps/webdesserts').exists())
        self.assert_no_staging()

    def test_staged_candidate_is_not_revalidated(self):
        self.validate()
        self.git('add', '--', gate.CASK)
        with self.assertRaises(gate.ValidationError):
            self.validate()
        self.assertFalse(any(a[:2] == ['git', 'push'] for a in self.calls))

    def test_validation_call_graph_excludes_publication_and_git_writes(self):
        module = ast.parse(Path(gate.__file__).read_text())
        functions = {node.name: node for node in module.body if isinstance(node, ast.FunctionDef)}
        reachable = set()
        def visit(name):
            if name in reachable or name not in functions:
                return
            reachable.add(name)
            for node in ast.walk(functions[name]):
                if isinstance(node, ast.Call) and isinstance(node.func, ast.Name):
                    visit(node.func.id)
        visit('validate')
        self.assertNotIn('publish', reachable)
        self.assertNotIn('remote_head', reachable)
        self.assertNotIn('check_queue', reachable)
        for verb in ['add', 'commit', 'push', 'config', 'reset', 'update-ref']:
            with self.subTest(verb=verb), self.assertRaises(gate.ValidationError):
                gate.read_git(self.tap, self.runner, verb)

    def test_existing_mapping_is_not_replaced(self):
        mapping = self.repo / 'Library/Taps/webdesserts/homebrew-tap'
        mapping.mkdir(parents=True)
        with self.assertRaises(gate.ValidationError):
            self.validate()
        self.assertTrue(mapping.is_dir())
        self.assert_no_staging()

    def test_publish_newer_success_cask_only(self):
        receipt = self.publish()
        self.assertEqual(receipt['remote'], 'accepted')
        self.assertEqual(self.git('show', '--format=', '--name-only', 'HEAD').stdout.strip(), gate.CASK)
        self.assertEqual(self.git('status', '--porcelain').stdout, '')
        self.assertEqual([a for a in self.calls if a[:2] == ['git', 'add']], [['git', 'add', '--', gate.CASK]])

    def test_publish_equal_or_older_refuses_before_rewrite(self):
        for version in ['0.5.8', '0.5.9']:
            path = self.tap / gate.CASK
            path.parent.mkdir(exist_ok=True)
            path.write_text(gate.generate('v' + version, 'a' * 64, 'sonoma'))
            self.git('add', '--', gate.CASK)
            self.git('commit', '-m', 'published fixture')
            self.baseline = self.git('rev-parse', 'HEAD').stdout.strip()
            self.git('update-ref', 'refs/remotes/origin/main', self.baseline)
            self.remote = self.baseline
            before = path.read_bytes()
            with self.subTest(version=version), self.assertRaises(gate.ValidationError):
                self.publish()
            self.assertEqual(path.read_bytes(), before)
            self.assert_no_staging()

    def test_publish_dirty_or_stale_refuses(self):
        self.remote = 'a' * 40
        with self.assertRaises(gate.ValidationError):
            self.publish()
        self.assertFalse((self.tap / gate.CASK).exists())
        self.remote = self.baseline
        self.validate()
        with self.assertRaises(gate.ValidationError):
            self.publish()
        self.assert_no_staging()

    def test_publish_gate_failure_does_not_stage(self):
        self.fail = 'style'
        with self.assertRaises(gate.ValidationError):
            self.publish()
        self.assert_no_staging()

    def test_commit_failure_reports_local_stage(self):
        self.fail = 'commit'
        receipt = self.publish()
        self.assertEqual(receipt['status'], 'publication_failed')
        self.assertEqual(receipt['local'], 'staging_or_commit_may_exist')
        self.assertFalse(any(a[:2] == ['git', 'push'] for a in self.calls))

    def test_push_failure_reports_remote_evidence_without_retry(self):
        for outcome, expected in [('rejected', 'unchanged'), ('unchanged-exception', 'unchanged'), ('accepted', 'accepted'), ('accepted-nonzero', 'accepted'), ('unknown', 'unknown'), ('diverged', 'unknown')]:
            with self.subTest(outcome=outcome):
                self.git('reset', '--hard', self.baseline)
                self.remote = self.baseline
                self.push = outcome
                self.calls.clear()
                receipt = self.publish()
                self.assertEqual(receipt['remote'], expected)
                self.assertEqual(receipt['local'], 'committed')
                self.assertNotEqual(receipt['commit'], self.baseline)
                self.assertEqual(len([a for a in self.calls if a[:2] == ['git', 'push']]), 1)
                self.assertFalse(any('--force' in a or 'rebase' in a for a in self.calls))


class AuditQueueTests(unittest.TestCase):
    def test_stable_only_numeric_ordering(self):
        for tag in ['v0.5.8-rc.1', 'v0.5.8+build.1', 'v01.5.8', '0.5.8', 'v0.5', 'v0.5.8\n']:
            with self.subTest(tag=tag), self.assertRaises(gate.ValidationError):
                gate.generate(tag, 'a' * 64, 'sonoma')
        for tag, baseline in [('v0.5.10', '0.5.9'), ('v0.6.0', '0.5.99'), ('v1.0.0', '0.99.99')]:
            gate.require_newer(tag, '  version "' + baseline + '"\n')
        for tag, baseline in [('v0.5.8', '0.5.8'), ('v0.5.9', '0.5.10'), ('v0.5.8', '0.5.7-rc.1')]:
            with self.subTest(tag=tag, baseline=baseline), self.assertRaises(gate.ValidationError):
                gate.require_newer(tag, '  version "' + baseline + '"\n')

    def test_audit_config_rejects_placeholders_and_missing_or_invalid_records(self):
        with tempfile.TemporaryDirectory(prefix='memory-audit-config-test-') as directory:
            path = Path(directory) / 'audit.json'
            records = [{}, {'exit_code': 0, 'stdout_lines': [], 'stderr_lines': ['fixture']},
                       {'exit_code': 1, 'stdout_lines': [], 'stderr_lines': []},
                       {'exit_code': 1, 'stdout_lines': [], 'stderr_lines': ['{path}']},
                       {'exit_code': 1, 'stdout_lines': [], 'stderr_lines': ['fixture\x85']},
                       {'exit_code': True, 'stdout_lines': [], 'stderr_lines': ['fixture']}]
            for record in records:
                path.write_text(json.dumps(record))
                with self.subTest(record=record), self.assertRaises(gate.ValidationError):
                    gate.load_audit_config(path)

    def test_exact_audit_classifier_only(self):
        record = {'exit_code': 1, 'stdout_lines': [], 'stderr_lines': ['fixture rejection']}
        for code, out, err in [(1, '', 'fixture rejection\n'), (1, '', 'fixture rejection\r\n')]:
            self.assertEqual(gate.classify_audit(subprocess.CompletedProcess([], code, out, err), record), 'expected_self_signed_incompatibility')
        for code, out, err in [(0, '', ''), (1, '', 'fixture rejection\nother'), (1, 'other', 'fixture rejection'), (1, '', 'fixture rejection\x85'), (2, '', 'fixture rejection')]:
            with self.subTest(code=code, out=out, err=err), self.assertRaises(gate.ValidationError):
                gate.classify_audit(subprocess.CompletedProcess([], code, out, err), record)

    def test_queue_self_exclusion_all_nonterminal_and_incomplete_errors(self):
        calls = []
        def runner(argv, **kw):
            calls.append(argv)
            status = argv[-1].split('status=')[1].split('&')[0]
            rows = [{'id': 123, 'status': status}]
            return subprocess.CompletedProcess(argv, 0, json.dumps([{'total_count': 1, 'workflow_runs': rows}]), '')
        gate.check_queue('123', runner=runner)
        self.assertEqual(len(calls), 2 * len(gate.NONTERMINAL))
        for state in gate.NONTERMINAL:
            def active(argv, **kw):
                status = argv[-1].split('status=')[1].split('&')[0]
                rows = [{'id': 456, 'status': state}] if status == state else []
                return subprocess.CompletedProcess(argv, 0, json.dumps([{'total_count': len(rows), 'workflow_runs': rows}]), '')
            with self.subTest(state=state), self.assertRaises(gate.ValidationError):
                gate.check_queue('123', runner=active)
        for code, payload in [(1, ''), (0, 'bad'), (0, '[]'), (0, '[{"total_count": 1, "workflow_runs": []}]')]:
            with self.subTest(payload=payload), self.assertRaises(gate.ValidationError):
                gate.check_queue('123', runner=lambda *a, **k: subprocess.CompletedProcess([], code, payload, ''))


if __name__ == '__main__':
    unittest.main()
