"""Run the real TUI against an isolated notification bus and native fixture."""
import argparse
import importlib.util
import json
import os
from pathlib import Path
import pty
import re
import fcntl
import select
import struct
import subprocess
import tempfile
import termios
import threading
import time


def main():
    parser=argparse.ArgumentParser()
    parser.add_argument('--binary',type=Path,required=True)
    parser.add_argument('--isolated',action='store_true')
    args=parser.parse_args()
    if not args.isolated:
        subprocess.run(['dbus-run-session','--','python',str(Path(__file__).resolve()),'--binary',str(args.binary.resolve()),'--isolated'],check=True)
        return
    import dbus.service
    from dbus.mainloop.glib import DBusGMainLoop
    from gi.repository import GLib
    DBusGMainLoop(set_as_default=True)
    received=[]
    class Notifications(dbus.service.Object):
        @dbus.service.method('org.freedesktop.Notifications',in_signature='susssasa{sv}i',out_signature='u')
        def Notify(self,app,replaces,icon,title,body,actions,hints,timeout):
            received.append(dict(app=str(app),title=str(title),body=str(body)))
            return len(received)
        @dbus.service.method('org.freedesktop.Notifications',out_signature='as')
        def GetCapabilities(self):return ['body','body-markup']
        @dbus.service.method('org.freedesktop.Notifications',out_signature='ssss')
        def GetServerInformation(self):return ('fixture','qqtui','1','1.2')
    bus=dbus.SessionBus()
    name=dbus.service.BusName('org.freedesktop.Notifications',bus)
    service=Notifications(bus,'/org/freedesktop/Notifications')
    loop=GLib.MainLoop();threading.Thread(target=loop.run,daemon=True).start()
    spec=importlib.util.spec_from_file_location('history_fixture',Path(__file__).with_name('tui-history.test.py'))
    history=importlib.util.module_from_spec(spec);spec.loader.exec_module(history)
    class Fixture(history.Fixture):
        mail='6';child='14'
        def call(self,method,params):
            if method=='conversations.snapshot':
                result=super().call(method,params)
                result['conversations'].extend([
                    dict(peer=dict(chat_type=103,uid='mail'),id='mail',name='Mail fixture',unread=self.mail,pinned=False),
                    dict(peer=dict(chat_type=7,uid=''),id='folder',name='Group assistant',unread='1' if self.child!='0' else '0',pinned=False,children=[dict(peer=dict(chat_type=2,uid='child'),id='child',name='Nested group',unread=self.child,pinned=False)])])
                result['unread_total']=self.mail
                return result
            if params.get('peer',{}).get('chat_type')==103:
                if method=='messages.read':
                    self.calls.append((method,params));self.mail='0';return {}
                if method=='messages.history':return dict(messages=[dict(history.message(1),peer=params['peer'],text='Mail Sender\nMail Subject\nMail summary\nhttps://mail.qq.com/')],has_before=False,has_after=False,anchor_found=True)
            if method=='conversations.read':
                self.calls.append((method,params))
                if params['id']=='mail':self.mail='0'
                if params['id']=='folder':self.child='0'
                return {}
            return super().call(method,params)
    with tempfile.TemporaryDirectory(prefix='qqtui-notifications-') as temporary:
        root=Path(temporary);env=dict(os.environ,TERM='xterm-256color')
        for key,value in [('XDG_CONFIG_HOME','config'),('XDG_STATE_HOME','state'),('XDG_CACHE_HOME','cache'),('XDG_RUNTIME_DIR','runtime')]:
            path=root/value;path.mkdir(mode=0o700);env[key]=str(path)
        runtime=root/'runtime/qqtui';runtime.mkdir(mode=0o700)
        server=Fixture(runtime/'bridge.sock');threading.Thread(target=server.serve_forever,daemon=True).start()
        master,slave=pty.openpty();fcntl.ioctl(slave,termios.TIOCSWINSZ,struct.pack('HHHH',40,120,960,800))
        process=subprocess.Popen([str(args.binary.resolve()),'--account','default'],stdin=slave,stdout=slave,stderr=slave,env=env,start_new_session=True);os.close(slave)
        transcript=bytearray();answered=False
        def drain(seconds):
            nonlocal answered
            until=time.monotonic()+seconds
            while time.monotonic()<until:
                if select.select([master],[],[],.05)[0]:
                    try:data=os.read(master,65536)
                    except OSError:break
                    transcript.extend(data)
                    if not answered and b'\x1b[5n' in transcript:
                        os.write(master,b'\x1b[?1;2c\x1b[6;20;8t\x1b[0n');answered=True
        def wait(test):
            until=time.monotonic()+12
            while not test():
                drain(.1)
                assert process.poll() is None,transcript.decode(errors='replace')[-500:]
                assert time.monotonic()<until,transcript.decode(errors='replace')[-1000:]
        def key(value):os.write(master,value);drain(.2)
        try:
            wait(lambda:b'[6] qqtui' in transcript)
            assert not received,'initial snapshot replayed old notifications'
            key(b' ');assert '会话动作' in re.sub(r'\x1b\[[0-?]*[ -/]*[@-~]','',transcript.decode(errors='replace'))
            key(b'\r');wait(lambda:any(m=='conversations.read' and p['id']=='fixture' for m,p in server.calls))
            key(b'jjj ');key(b'\r');wait(lambda:server.mail=='0')
            wait(lambda:b'[0] qqtui' in transcript)
            server.mail='6'
            key(b'\r');wait(lambda:b'Mail Subject' in transcript)
            key(b'\x1b[I')
            wait(lambda:any(m=='messages.read' and p['peer']['chat_type']==103 for m,p in server.calls))
            assert server.mail=='0'
            key(b'i');assert 'QQ 邮箱提醒仅支持查看' in re.sub(r'\x1b\[[0-?]*[ -/]*[@-~]','',transcript.decode(errors='replace'))
            key(b'hj ');key(b'\r');wait(lambda:server.child=='0')
            wait(lambda:b'[0] qqtui' in transcript)
            with server.lock:
                server.sequence+=1
                event=dict(epoch='fixture-host',account='fixture',seq=server.sequence,kind='notification',notification=dict(items=[dict(key='mail',title='[6] QQ · fixture · Mail',body='<b>mail & text</b>')]))
                for client in server.clients:client.write(dict(jsonrpc='2.0',method='event',params=event))
            wait(lambda:len(received)==1)
            assert received[0]['app']=='qqtui'
            assert received[0]['title'].startswith('[6]')
            assert received[0]['body']=='&lt;b&gt;mail &amp; text&lt;/b&gt;'
            key(b'q');assert process.wait(timeout=5)==0
            print('Desktop bus notification, escaped content, unread window title, and mailbox/group assistant conversation menus passed')
        finally:
            if process.poll() is None:process.terminate();process.wait(timeout=5)
            os.close(master);server.shutdown();server.server_close();loop.quit()

if __name__=='__main__':main()
