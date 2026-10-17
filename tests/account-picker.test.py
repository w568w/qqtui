"""Exercise startup account selection without starting or logging into QQ."""
import argparse,fcntl,json,os,pty,re,select,struct,subprocess,tempfile,termios,time
from pathlib import Path

parser=argparse.ArgumentParser();parser.add_argument('--binary',type=Path,required=True)
binary=parser.parse_args().binary.resolve()
with tempfile.TemporaryDirectory(prefix='qqtui-account-picker-') as temp:
    root=Path(temp);env=dict(os.environ,TERM='xterm-256color')
    for key,name in [('XDG_CONFIG_HOME','config'),('XDG_STATE_HOME','state'),('XDG_CACHE_HOME','cache'),('XDG_RUNTIME_DIR','runtime')]:
        path=root/name;path.mkdir(mode=0o700);env[key]=str(path)
    (root/'config/qqtui/accounts/123456789').mkdir(parents=True)
    listed=json.loads(subprocess.check_output([binary,'accounts'],env=env,text=True))
    assert [item['account'] for item in listed]==[None,'123456789']
    master,slave=pty.openpty();fcntl.ioctl(slave,termios.TIOCSWINSZ,struct.pack('HHHH',30,100,900,600))
    process=subprocess.Popen([binary],stdin=slave,stdout=slave,stderr=slave,env=env,start_new_session=True);os.close(slave)
    output=b''
    def until(text):
        global output
        deadline=time.monotonic()+8
        def emitted_text():
            plain=re.sub(rb'\x1b\[[0-?]*[ -/]*[@-~]',b'',output).decode(errors='replace')
            return ''.join(plain.split())
        while ''.join(text.split()) not in emitted_text():
            if time.monotonic()>deadline:raise AssertionError(output.decode(errors='replace'))
            if select.select([master],[],[],.1)[0]:output+=os.read(master,65536)
    try:
        until('选择 QQ 账号');until('123456789')
        os.write(master,b'a');until('QQ 号')
        os.write(master,b'01\r');until('请输入有效')
        os.write(master,b'\x1b');time.sleep(.1);os.write(master,b'q')
        assert process.wait(timeout=5)==0
        assert not (root/'state/qqtui').exists(),'Canceling the picker started or created a QQ host'
        print('Startup chooser: remembered account, add-account validation, cancel and clean terminal exit passed')
        for command in [['status'], ['doctor'], ['backend','start'], ['backend','stop'], ['backend','view'], ['backend','logs']]:
            master2,slave=pty.openpty();fcntl.ioctl(slave,termios.TIOCSWINSZ,struct.pack('HHHH',30,100,900,600))
            child=subprocess.Popen([binary,*command],stdin=slave,stdout=slave,stderr=slave,env=env,start_new_session=True);os.close(slave)
            original_master=master;master=master2;output=b''
            try:
                until('选择 QQ 宿主');until('123456789')
                os.write(master,b'q');assert child.wait(timeout=5)==0
                assert not (root/'state/qqtui').exists(),f'{command} cancellation changed host state'
            finally:
                if child.poll() is None:child.kill();child.wait()
                os.close(master);master=original_master
        print('Backend commands: shared chooser and cancellation passed for status, doctor, start, stop, view and logs')
        log=root/'state/qqtui/accounts/123456789/host.log';log.parent.mkdir(parents=True);log.write_text('selected-backend-log\n')
        master2,slave=pty.openpty();fcntl.ioctl(slave,termios.TIOCSWINSZ,struct.pack('HHHH',30,100,900,600))
        child=subprocess.Popen([binary,'backend','logs'],stdin=slave,stdout=slave,stderr=slave,env=env,start_new_session=True);os.close(slave)
        original_master=master;master=master2;output=b''
        try:
            until('选择 QQ 宿主');os.write(master,b'j\r');until('selected-backend-log')
            assert child.wait(timeout=5)==0
        finally:
            if child.poll() is None:child.kill();child.wait()
            os.close(master);master=original_master
        bypass=subprocess.run([binary,'--account','123456789','backend','logs'],capture_output=True,text=True,env=env,timeout=5)
        assert bypass.returncode==0 and bypass.stdout.strip()=='selected-backend-log'
        print('Backend selection routes logs to the chosen account; --account bypasses the picker')
    finally:
        if process.poll() is None:process.kill();process.wait()
        os.close(master)
