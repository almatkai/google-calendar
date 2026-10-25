#!/usr/bin/env python3
"""Actual JSONL child/loopback fixtures. No Google login, browser or clipboard actions."""
import argparse
import base64
import hashlib
import http.client
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import os
from pathlib import Path
import queue
import socket
import subprocess
import tempfile
import threading
import time
from urllib.parse import parse_qs, urlsplit, urlencode
import uuid

COMMANDS = ['create-event', 'create-quick-event', 'search-contacts', 'list-events', 'list-calendars']
CHECKS = []


def check(condition, label):
    assert condition, label
    CHECKS.append(label)


def frame_summary(frame):
    """Diagnose fixture failures without dumping OAuth URLs, tokens or views."""
    summary = {key: frame[key] for key in ('type', 'method') if key in frame}
    if isinstance(frame.get('error'), dict):
        summary['error'] = {key: frame['error'][key] for key in ('code', 'message') if key in frame['error']}
    if isinstance(frame.get('result'), dict):
        summary['resultKeys'] = sorted(frame['result'])
    return json.dumps(summary, sort_keys=True)


class Host:
    def __init__(self, binary, directory, origin=None, client=False, migration=None, platform=None):
        self.root = Path(directory)
        self.root.mkdir(parents=True, exist_ok=True)
        # Python uppercases os.environ keys on Windows; preserve the OS root for Winsock.
        env = {k.upper(): v for k, v in os.environ.items() if k.upper() in ('SYSTEMROOT', 'WINDIR', 'TEMP', 'TMP')}
        if os.name == 'nt' and not env.get('SYSTEMROOT'):
            raise RuntimeError('Windows fixture host requires a nonempty SystemRoot')
        env['PATH'] = os.path.join(env['SYSTEMROOT'], 'System32') if os.name == 'nt' else '/usr/bin:/bin'
        if origin:
            env['TEZBAR_GOOGLE_FIXTURE_ORIGIN'] = origin
        if client:
            env['TEZBAR_GOOGLE_OAUTH_CLIENT_ID'] = 'fixture-desktop.apps.googleusercontent.com'
        self.p = subprocess.Popen([str(Path(binary).resolve())], stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                                  stderr=subprocess.PIPE, cwd=self.root, env=env)
        self.messages = queue.Queue(maxsize=128)
        def read():
            for line in self.p.stdout:
                self.messages.put(json.loads(line))
            self.messages.put(None)
        self.reader = threading.Thread(target=read, daemon=True)
        self.reader.start()
        hello = self.recv()
        check(hello['type'] == 'hello' and hello['protocol'] == 1 and hello['id'] == 'com.tezbar.native.google-calendar', 'hello')
        self.initialized = self.call('initialize', {'protocol': 1, 'extensionId': hello['id'],
            'platform': platform or ('Windows' if os.name == 'nt' else 'macOS'), 'packageDir': str(Path(binary).resolve().parent.parent),
            'dataDir': str(self.root / 'data'), 'tempDir': str(self.root / 'temp'),
            'grantedCapabilities': ['shell.open', 'clipboard.write'], 'reason': 'fixture', 'migrationPaths': migration or {}})
    def recv(self):
        value = self.messages.get(timeout=8)
        assert value is not None, 'unexpected EOF'
        return value
    def send(self, value):
        self.p.stdin.write((json.dumps(value) + '\n').encode())
        self.p.stdin.flush()
    def request(self, method, params):
        identity = 'h:' + str(uuid.uuid4())
        self.send({'type': 'request', 'id': identity, 'method': method, 'params': params})
        return identity
    def result(self, identity):
        value = self.recv()
        if value.get('type') == 'cancel':
            value = self.recv()
        assert value.get('id') == identity, value
        return value
    def call(self, method, params):
        return self.result(self.request(method, params))
    def open(self, session, command, preferences=None, launch_context=None):
        return self.call('session.open', {'sessionId': session, 'commandName': command, 'preferences': preferences or {}, 'argumentValues': {}, 'launchContext': launch_context or {}})
    def action(self, session, action, values=None, **extra):
        return self.call('session.call', {'sessionId': session, 'operation': 'extension:invoke-action',
                 'payload': {'actionId': action, 'formValues': values or {}, **extra}})
    def close(self, shutdown=True):
        if self.p.poll() is None:
            try:
                if shutdown:
                    check('result' in self.call('shutdown', {}), 'shutdown response')
                else:
                    self.p.stdin.close()
                check(self.p.wait(timeout=6) == 0, 'bounded shutdown/EOF')
                check(self.p.stderr.read() == b'', 'no credential logs')
            finally:
                if self.p.poll() is None:
                    self.p.kill()
                    self.p.wait(timeout=5)
        if not self.p.stdin.closed:
            self.p.stdin.close()
        self.p.stdout.close()
        self.p.stderr.close()
        self.reader.join(timeout=1)


class Fixture:
    def __init__(self):
        self.seen = []
        self.token_count = 0
        self.token_delay = 0
        self.api_error = None
        self.search_started = threading.Event()
        self.search_release = threading.Event()
        owner = self
        class Handler(BaseHTTPRequestHandler):
            def log_message(self, *_): pass
            def do_GET(self): self.handle_api()
            def do_POST(self): self.handle_api()
            def do_PATCH(self): self.handle_api()
            def do_DELETE(self): self.handle_api()
            def handle_api(self):
                parsed = urlsplit(self.path)
                q = parse_qs(parsed.query)
                body = self.rfile.read(int(self.headers.get('Content-Length', 0)))
                owner.seen.append((self.command, parsed.path, q, body, self.headers.get('Authorization')))
                status = 200
                if parsed.path == '/token':
                    owner.token_count += 1
                    time.sleep(owner.token_delay)
                    data = {'access_token': 'fixture-access', 'refresh_token': 'fixture-refresh', 'expires_in': 3600}
                elif owner.api_error:
                    status, data = owner.api_error
                elif parsed.path.endswith('/calendarList'):
                    data = {'items': [{'id': 'primary', 'summary': 'Fixture Calendar', 'accessRole': 'owner'}]}
                    if 'pageToken' not in q: data['nextPageToken'] = 'calendar-next'
                elif parsed.path.startswith('/v1/'):
                    query = q.get('query', [''])[0]
                    if query == 'slow':
                        owner.search_started.set()
                        owner.search_release.wait(timeout=6)
                    data = {'results': [{'person': {'resourceName': 'people/' + query, 'emailAddresses': [{'value': query + '@example.test'}], 'names': [{'displayName': query}]}}]}
                elif self.command in ('POST', 'PATCH'):
                    data = {'id': 'created', 'summary': 'Fixture Created', 'htmlLink': 'https://calendar.google.com/fixture-created'}
                elif self.command == 'DELETE':
                    data = {}
                else:
                    data = {'items': [{'id': 'event-1', 'summary': 'Fixture Event', 'htmlLink': 'https://calendar.google.com/fixture-event',
                        'start': {'date': '2027-01-01'}, 'end': {'date': '2027-01-02'}, 'location': 'Fixture Location',
                        'conferenceData': {'entryPoints': [{'uri': 'https://meet.google.com/fixture'}]},
                        'attendees': [{'self': True, 'email': 'fixture@example.test', 'responseStatus': 'needsAction'}]}]}
                    if 'pageToken' not in q: data['nextPageToken'] = 'events-next'
                encoded = json.dumps(data).encode()
                try:
                    self.send_response(status)
                    self.send_header('Content-Type', 'application/json')
                    self.send_header('Content-Length', str(len(encoded)))
                    self.end_headers()
                    self.wfile.write(encoded)
                except (BrokenPipeError, ConnectionResetError): pass
        self.server = ThreadingHTTPServer(('127.0.0.1', 0), Handler)
        self.server.daemon_threads = True
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)
        self.thread.start()
        self.origin = 'http://127.0.0.1:' + str(self.server.server_port)
    def close(self):
        self.search_release.set()
        self.server.shutdown()
        self.server.server_close()
        self.thread.join(timeout=2)


def callback(url, state, code='fixture-code', fragmented=False):
    parsed = urlsplit(url)
    conn = socket.create_connection(('127.0.0.1', parsed.port), timeout=3)
    request = f'GET {parsed.path}?{urlencode({"state": state, "code": code})} HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n'.encode()
    if fragmented:
        conn.sendall(request[:10])
        time.sleep(.03)
        conn.sendall(request[10:])
    else: conn.sendall(request)
    reply = conn.recv(8192)
    conn.close()
    return reply


def disconnected(url):
    try:
        s = socket.create_connection(('127.0.0.1', urlsplit(url).port), timeout=.3)
        s.close()
        return False
    except OSError:
        return True


def offline(binary, root):
    h = Host(binary, root / 'offline')
    try:
        check(h.initialized.get('result', {}).get('ready') is True, 'initialize')
        for command in COMMANDS:
            result = h.open(command, command)
            check(result['result']['title'] == 'Connect Google Calendar', 'offline ' + command)
        check('error' in h.action('list-events', 'google:connect'), 'missing Desktop client rejected')
        check('result' in h.call('session.dispose', {'sessionId': 'list-events'}), 'dispose')
        check('error' in h.call('session.call', {'sessionId': 'list-events', 'operation': 'extension:refresh-session'}), 'disposed session rejected')
    finally: h.close()
    data = root / 'corrupt' / 'data' / 'google-oauth'
    data.mkdir(parents=True)
    (data / 'storage.json').write_text('{bad')
    h = Host(binary, root / 'corrupt')
    try:
        check('error' in h.initialized, 'corrupt credentials rejected')
        check((data / 'storage.json').read_text() == '{bad', 'corruption preserved')
    finally: h.close(False)
    legacy = root / 'legacy.json'
    legacy.write_text('[]')
    h = Host(binary, root / 'migration-bad', migration={'nativeGoogleOAuth': str(legacy)})
    try: check('error' in h.initialized, 'malformed migration rejected')
    finally: h.close(False)
    legacy.write_text('{"refresh_token":"fixture-only","expires_at":0,"unrelatedSecret":"must-not-import"}')
    h = Host(binary, root / 'migration-ok', migration={'nativeGoogleOAuth': str(legacy)})
    try:
        check('result' in h.initialized, 'explicit migration accepted')
        saved = json.loads((h.root / 'data/google-oauth/storage.json').read_text())
        check(saved['refresh_token'] == 'fixture-only' and 'unrelatedSecret' not in saved, 'migration imports exact token keys')
        if os.name != 'nt':
            check((h.root / 'data/google-oauth/storage.json').stat().st_mode & 0o777 == 0o600, 'private token permissions')
    finally: h.close()
    legacy.write_text('{bad')
    h = Host(binary, root / 'migration-ok', migration={'nativeGoogleOAuth': str(legacy)})
    try: check('result' in h.initialized, 'migration once; private store authoritative')
    finally: h.close()


def oauth_browser_request(h):
    check('result' in h.initialized, 'fixture initialize: ' + frame_summary(h.initialized))
    opened = h.open('auth', 'list-events', {'showLocation': True}, {'calendarId': 'team@example.test'})
    check(isinstance(opened.get('result'), dict) and opened['result'].get('title') == 'Connect Google Calendar',
          'OAuth session.open: ' + frame_summary(opened))
    request = h.request('session.call', {'sessionId': 'auth', 'operation': 'extension:invoke-action', 'payload': {'actionId': 'google:connect'}})
    reverse = h.recv()
    check(reverse.get('type') == 'request' and reverse.get('method') == 'host.shell.open',
          'typed OAuth browser request: ' + frame_summary(reverse))
    return request, reverse


def oauth_probe(binary, root, platform=None):
    f = Fixture()
    try:
        h = Host(binary, root / 'oauth-probe', f.origin, True, platform=platform)
        try:
            oauth_browser_request(h)
        finally: h.close(False)
    finally: f.close()


def network(binary, root):
    f = Fixture()
    h = Host(binary, root / 'network', f.origin, True)
    try:
        request, reverse = oauth_browser_request(h)
        auth = parse_qs(urlsplit(reverse['params']['target']).query)
        check(auth['code_challenge_method'] == ['S256'] and auth['access_type'] == ['offline'], 'PKCE and offline consent')
        redirect = auth['redirect_uri'][0]
        h.send({'type': 'response', 'id': reverse['id'], 'result': None})
        check(b'400' in callback(redirect, 'wrong-state'), 'wrong OAuth state rejected')
        check(f.token_count == 0, 'invalid state never exchanges code')
        check(b'200' in callback(redirect, auth['state'][0], fragmented=True), 'fragmented loopback callback')
        check(h.result(request)['result']['title'] == 'Upcoming Events', 'OAuth to event view')
        check(any('/calendars/team%40example.test/events' in x[1] for x in f.seen), 'launch context survives authorization')
        token_form = parse_qs(next(x[3].decode() for x in f.seen if x[1] == '/token'))
        challenge = base64.urlsafe_b64encode(hashlib.sha256(token_form['code_verifier'][0].encode()).digest()).decode().rstrip('=')
        check(challenge == auth['code_challenge'][0] and token_form['redirect_uri'] == [redirect], 'exchanged verifier and redirect match')
        check(disconnected(redirect), 'listener closes after OAuth')
        check(any(x[2].get('pageToken') == ['calendar-next'] for x in f.seen), 'calendar pagination')
        check(all(x[4] == 'Bearer fixture-access' for x in f.seen if x[1] != '/token'), 'API bearer token')
        check('result' in h.call('session.call', {'sessionId': 'auth', 'operation': 'extension:load-more'}), 'events load more')
        check(any(x[2].get('pageToken') == ['events-next'] for x in f.seen), 'events cursor')
        # Store snapshots are host supplied. Spoofed session.call preferences cannot change create defaults.
        check(h.open('create', 'create-event', {'addSignature': False, 'sendInvitations': 'none', 'defaultEventDuration': '45'})['result']['root']['type'] == 'Form', 'event form')
        created = h.action('create', 'google:submit', {'title': 'Local Fixture', 'startDate': '2027-01-01T10:00:00Z'}, preferences={'addSignature': True, 'sendInvitations': 'all'})
        check(created['result']['title'] == 'Created Event', 'actual create fixture')
        write = next(x for x in f.seen if x[0] == 'POST' and x[1].endswith('/events'))
        body = json.loads(write[3])
        check(body['description'] == '' and write[2]['sendUpdates'] == ['none'] and body['end']['dateTime'].startswith('2027-01-01T10:45'), 'fenced preference snapshot')
        check(h.action('create', 'google:created:delete')['result']['root']['type'] == 'Form', 'delete confirmation')
        check('error' in h.action('create', 'google:confirm', {'confirmed': False}), 'unconfirmed deletion rejected')
        check(h.action('create', 'google:confirm', {'confirmed': True})['result']['mode'] == 'no-view', 'confirmed fixture delete')
        h.open('quick', 'create-quick-event')
        check(h.action('quick', 'google:submit', {'input': 'Fixture tomorrow 10am'})['result']['title'] == 'Created Event', 'quickAdd fixture')
        check(any(x[1].endswith('/quickAdd') and x[2]['text'] == ['Fixture tomorrow 10am'] for x in f.seen), 'quickAdd API query')
        h.open('calendars', 'list-calendars')
        check(h.action('calendars', 'google:calendar:0')['result']['title'] == 'Upcoming Events', 'calendar navigation')
        check(h.action('auth', 'google:event:respond:0')['result']['root']['type'] == 'Form', 'RSVP confirmation')
        check('result' in h.action('auth', 'google:confirm', {'status': 'accepted'}), 'RSVP fixture patch')
        patch = next(x for x in f.seen if x[0] == 'PATCH')
        check(json.loads(patch[3])['attendees'][0]['responseStatus'] == 'accepted', 'self attendee response')
        h.open('contacts', 'search-contacts')
        check('result' in h.call('session.call', {'sessionId': 'contacts', 'operation': 'extension:search-text-changed', 'payload': {'searchText': 'Ada'}}), 'People and other Contacts search')
        check(any(x[1].startswith('/v1/') and x[2].get('readMask') == ['names,emailAddresses'] for x in f.seen), 'otherContacts valid read mask')
        check({x[1] for x in f.seen if x[1].startswith('/v1/')} == {'/v1/people:searchContacts', '/v1/otherContacts:search'}, 'both People endpoints')
        identity = h.request('session.call', {'sessionId': 'contacts', 'operation': 'extension:invoke-action', 'payload': {'actionId': 'google:contact:copy:0'}})
        reverse = h.recv()
        check(reverse['method'] == 'host.clipboard.write' and reverse['params'] == {'text': 'Ada@example.test'}, 'typed clipboard fixture only')
        h.send({'type': 'response', 'id': reverse['id'], 'result': None})
        check('result' in h.result(identity), 'clipboard acknowledgement')
        slow = h.request('session.call', {'sessionId': 'contacts', 'operation': 'extension:search-text-changed', 'payload': {'searchText': 'slow'}})
        check(f.search_started.wait(timeout=5), 'slow search in flight')
        fast = h.request('session.call', {'sessionId': 'contacts', 'operation': 'extension:search-text-changed', 'payload': {'searchText': 'latest'}})
        responses = [h.recv(), h.recv()]
        byid = {x['id']: x for x in responses}
        check('error' in byid[slow] and 'latest@example.test' in json.dumps(byid[fast]['result']), 'latest search wins')
        f.search_release.set()
        f.api_error = (403, {'error': {'message': 'fixture permission denied'}})
        check('fixture permission denied' in h.call('session.call', {'sessionId': 'auth', 'operation': 'extension:refresh-session'})['error']['message'], 'Calendar error translation')
        f.api_error = None
        # Reconnect owns its listener and mutex; cancellation/dispose closes both and next session remains usable.
        for mode in ('cancel', 'dispose'):
            session = 'pending-' + mode
            h.open(session, 'list-events')
            pending = h.request('session.call', {'sessionId': session, 'operation': 'extension:invoke-action', 'payload': {'actionId': 'google:connect'}})
            reverse = h.recv()
            redirect = parse_qs(urlsplit(reverse['params']['target']).query)['redirect_uri'][0]
            h.send({'type': 'response', 'id': reverse['id'], 'result': None})
            if mode == 'cancel':
                h.send({'type': 'cancel', 'requestId': pending, 'reason': 'fixture'})
                check('error' in h.result(pending), 'cancel authorization response')
            else:
                disposal = h.request('session.dispose', {'sessionId': session})
                replies = []
                for _ in range(3):
                    reply = h.recv()
                    if reply.get('type') == 'cancel':
                        check(reply['requestId'] == reverse['id'], 'cancel reverse RPC ownership')
                    else:
                        replies.append(reply)
                    if {x.get('id') for x in replies} == {pending, disposal}: break
                check(any(x.get('id') == disposal and 'result' in x for x in replies), 'dispose authorization response')
                check(any(x.get('id') == pending and 'error' in x for x in replies), 'dispose cancels authorization')
            check(disconnected(redirect), 'cancel/dispose listener closed')
        check(h.open('after-cancel', 'list-calendars')['result']['title'] == 'Google Calendars', 'auth mutex released')
    finally:
        h.close()
        f.close()
    # Two simultaneous sessions sharing expired credentials trigger only one refresh.
    f = Fixture()
    f.token_delay = .15
    root2 = root / 'refresh'
    token_dir = root2 / 'data/google-oauth'
    token_dir.mkdir(parents=True)
    (token_dir / 'storage.json').write_text(json.dumps({'access_token': 'expired', 'refresh_token': 'fixture-refresh', 'expires_at': 0}))
    h = Host(binary, root2, f.origin, True)
    try:
        ids = [h.request('session.open', {'sessionId': str(i), 'commandName': 'list-calendars', 'preferences': {}, 'argumentValues': {}}) for i in range(2)]
        replies = [h.recv(), h.recv()]
        check({x['id'] for x in replies} == set(ids) and all('result' in x for x in replies), 'concurrent sessions')
        check(f.token_count == 1, 'single flight refresh across sessions')
        saved = json.loads((token_dir / 'storage.json').read_text())
        check(saved['access_token'] == 'fixture-access' and saved['refresh_token'] == 'fixture-refresh', 'refresh persistence')
    finally:
        h.close(False)
        f.close()
    # EOF during a real loopback listener must exit without waiting for 180s.
    f = Fixture()
    h = Host(binary, root / 'eof-oauth', f.origin, True)
    try:
        h.open('eof', 'list-events')
        h.request('session.call', {'sessionId': 'eof', 'operation': 'extension:invoke-action', 'payload': {'actionId': 'google:connect'}})
        reverse = h.recv()
        redirect = parse_qs(urlsplit(reverse['params']['target']).query)['redirect_uri'][0]
        h.send({'type': 'response', 'id': reverse['id'], 'result': None})
        h.close(False)
        check(disconnected(redirect), 'EOF listener cleanup')
    finally:
        if h.p.poll() is None: h.close(False)
        f.close()


def run(binary, fixture=False, probe=False, platform=None):
    with tempfile.TemporaryDirectory(prefix='calendar-protocol-') as d:
        root = Path(d)
        info = json.loads(subprocess.check_output([str(Path(binary).resolve()), '--protocol-info'], timeout=5))
        build = json.loads(subprocess.check_output([str(Path(binary).resolve()), '--build-info'], timeout=5))
        check(build['fixtureNetwork'] is fixture, 'fixture feature selection')
        check(info['commands'] == COMMANDS and info['version'] == '1.4.4', 'command inventory')
        if probe:
            oauth_probe(binary, root, platform)
        else:
            offline(binary, root)
            if fixture: network(binary, root)
    print(json.dumps({'passed': True, 'assertions': len(CHECKS), 'checks': CHECKS, 'localFixturesOnly': True}))


if __name__ == '__main__':
    parser = argparse.ArgumentParser()
    parser.add_argument('--binary', required=True)
    parser.add_argument('--fixture-network', action='store_true')
    parser.add_argument('--oauth-probe', action='store_true', help='Only check initialize, session.open and the first OAuth frame')
    parser.add_argument('--host-platform', choices=['macOS', 'Windows'], help='Override initialize.platform for --oauth-probe; does not emulate the OS')
    args = parser.parse_args()
    if args.oauth_probe and not args.fixture_network:
        parser.error('--oauth-probe requires --fixture-network')
    if args.host_platform and not args.oauth_probe:
        parser.error('--host-platform requires --oauth-probe')
    run(args.binary, args.fixture_network, args.oauth_probe, args.host_platform)
