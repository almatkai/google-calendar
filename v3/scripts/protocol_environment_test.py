#!/usr/bin/env python3
"""Check the actual Host launch environment without requiring a Windows machine."""
import ntpath
from pathlib import Path
import posixpath
import sys
import tempfile
from types import SimpleNamespace
import unittest
from unittest.mock import patch

import protocol_test


class LaunchCaptured(Exception):
    pass


class HostEnvironmentTests(unittest.TestCase):
    def launch_environment(self, source, windows=True, **kwargs):
        # Replace only protocol_test's OS reference, not the process-wide os.name:
        # pathlib and tempfile must continue to use the real host platform.
        host_os = SimpleNamespace(environ=source, name='nt' if windows else 'posix',
                                  path=ntpath if windows else posixpath)
        with tempfile.TemporaryDirectory() as directory, \
                patch.object(protocol_test, 'os', host_os), \
                patch.object(protocol_test.subprocess, 'Popen', side_effect=LaunchCaptured) as popen:
            with self.assertRaises(LaunchCaptured):
                protocol_test.Host(sys.executable, Path(directory), **kwargs)
            self.assertEqual(popen.call_count, 1)
            return popen.call_args.kwargs['env']

    def test_uppercase_windows_environment_reaches_launch(self):
        env = self.launch_environment({
            'SYSTEMROOT': r'D:\Fixture Windows', 'WINDIR': r'D:\Fixture Windows',
            'TEMP': r'D:\Temp', 'TMP': r'D:\Tmp', 'PATH': r'Z:\Developer Tools',
            'HOME': 'private-home', 'USERPROFILE': 'private-profile',
            'GITHUB_TOKEN': 'private-token', 'TEZBAR_GOOGLE_OAUTH_CLIENT_ID': 'private-client',
            'TEZBAR_GOOGLE_FIXTURE_ORIGIN': 'https://private-origin.invalid',
        })
        self.assertEqual(env, {
            'SYSTEMROOT': r'D:\Fixture Windows', 'WINDIR': r'D:\Fixture Windows',
            'TEMP': r'D:\Temp', 'TMP': r'D:\Tmp', 'PATH': r'D:\Fixture Windows\System32',
        })

    def test_mixed_case_windows_environment_reaches_launch(self):
        env = self.launch_environment({
            'SystemRoot': r'E:\Windows', 'windir': r'E:\Windows',
            'Temp': r'E:\Temp', 'tMp': r'E:\Tmp', 'Path': r'Z:\Developer Tools',
        })
        self.assertEqual(env, {
            'SYSTEMROOT': r'E:\Windows', 'WINDIR': r'E:\Windows',
            'TEMP': r'E:\Temp', 'TMP': r'E:\Tmp', 'PATH': r'E:\Windows\System32',
        })

    def test_windows_requires_nonempty_system_root_before_launch(self):
        for source in ({'WINDIR': r'C:\Windows'}, {'SYSTEMROOT': '', 'WINDIR': r'C:\Windows'}):
            with self.subTest(source=source), tempfile.TemporaryDirectory() as directory, \
                    patch.object(protocol_test, 'os', SimpleNamespace(environ=source, name='nt', path=ntpath)), \
                    patch.object(protocol_test.subprocess, 'Popen', side_effect=AssertionError('unexpected process launch')) as popen:
                with self.assertRaisesRegex(RuntimeError, 'SystemRoot'):
                    protocol_test.Host(sys.executable, Path(directory))
                popen.assert_not_called()

    def test_posix_environment_keeps_os_only_path_without_windows_root(self):
        env = self.launch_environment({'PATH': '/private/tools', 'HOME': '/private/home',
                                       'GITHUB_TOKEN': 'private-token', 'TMP': '/tmp'}, windows=False)
        self.assertEqual(env, {'TMP': '/tmp', 'PATH': '/usr/bin:/bin'})

    def test_fixture_configuration_is_explicit(self):
        env = self.launch_environment({'SYSTEMROOT': r'C:\Windows',
                                       'TEZBAR_GOOGLE_OAUTH_CLIENT_ID': 'private-client',
                                       'TEZBAR_GOOGLE_FIXTURE_ORIGIN': 'https://private-origin.invalid'},
                                      origin='http://127.0.0.1:12345', client=True)
        self.assertEqual(env, {
            'SYSTEMROOT': r'C:\Windows', 'PATH': r'C:\Windows\System32',
            'TEZBAR_GOOGLE_FIXTURE_ORIGIN': 'http://127.0.0.1:12345',
            'TEZBAR_GOOGLE_OAUTH_CLIENT_ID': 'fixture-desktop.apps.googleusercontent.com',
        })


if __name__ == '__main__':
    unittest.main()
