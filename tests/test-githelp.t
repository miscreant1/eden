  $ extpath=$(dirname $TESTDIR)
  $ cp $extpath/githelp.py $TESTTMP # use $TESTTMP substitution in message
  $ cat >> $HGRCPATH << EOF
  > [extensions]
  > githelp=$TESTTMP/githelp.py
  > EOF

  $ hg init repo
  $ cd repo
  $ echo foo > test_file
  $ mkdir dir
  $ echo foo > dir/file
  $ echo foo > removed_file
  $ echo foo > deleted_file
  $ hg add -q .
  $ hg commit -m 'bar'
  $ hg bookmark both
  $ touch both
  $ touch untracked_file
  $ hg remove removed_file
  $ rm deleted_file

githelp on a single command should succeed
  $ hg githelp -- commit
  hg commit

githelp on a command with options should succeed
  $ hg githelp -- commit -pm "abc"
  hg record -m 'abc'

githelp on a command with standalone unrecognized option should succeed with warning
  $ hg githelp -- commit -p -v
  ignoring unknown option -v
  hg record

githelp on a command with unrecognized option packed with other options should fail with error
  $ hg githelp -- commit -pv
  abort: unknown option v packed with other options
  Please try passing the option as it's own flag: -v
  
  If this is a valid git command, please log a task for the source_control oncall.
  
  [255]
githelp for git rebase --skip
  $ hg githelp -- git rebase --skip
  hg revert --all -r .
  hg rebase --continue

githelp for git commit --amend (hg commit --amend pulls up an editor)
  $ hg githelp -- commit --amend
  hg commit --amend

githelp for git commit --amend --no-edit (hg amend does not pull up an editor)
  $ hg githelp -- commit --amend --no-edit
  hg amend

githelp for git checkout -- . (checking out a directory)
  $ hg githelp -- checkout -- .
  note: use --no-backup to avoid creating .orig files
  
  hg revert .

githelp for git checkout HEAD^ (should still work to pass a rev)
  $ hg githelp -- checkout HEAD^
  hg update .^

githelp checkout: args after -- should be treated as paths no matter what
  $ hg githelp -- checkout -- HEAD
  note: use --no-backup to avoid creating .orig files
  
  hg revert HEAD

githelp for git checkout with rev and path
  $ hg githelp -- checkout HEAD^ -- file.txt
  note: use --no-backup to avoid creating .orig files
  
  hg revert -r .^ file.txt

githelp for git with rev and path, without separator
  $ hg githelp -- checkout HEAD^ file.txt
  note: use --no-backup to avoid creating .orig files
  
  hg revert -r .^ file.txt

githelp for checkout with a file as first argument
  $ hg githelp -- checkout test_file
  note: use --no-backup to avoid creating .orig files
  
  hg revert test_file

githelp for checkout with a removed file as first argument
  $ hg githelp -- checkout removed_file
  note: use --no-backup to avoid creating .orig files
  
  hg revert removed_file

githelp for checkout with a deleted file as first argument
  $ hg githelp -- checkout deleted_file
  note: use --no-backup to avoid creating .orig files
  
  hg revert deleted_file

githelp for checkout with a untracked file as first argument
  $ hg githelp -- checkout untracked_file
  note: use --no-backup to avoid creating .orig files
  
  hg revert untracked_file

githelp for checkout with a directory as first argument
  $ hg githelp -- checkout dir
  note: use --no-backup to avoid creating .orig files
  
  hg revert dir

githelp for checkout when not in repo root
  $ cd dir
  $ hg githelp -- checkout file
  note: use --no-backup to avoid creating .orig files
  
  hg revert file

githelp for checkout with an argument that's both a file and a revision
  $ hg githelp -- checkout both
  hg update both
