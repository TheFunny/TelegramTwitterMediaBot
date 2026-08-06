#!/bin/bash

set -e
if [ "$(id -u)" -eq '0' ]
then
   USER_ID=${LOCAL_USER_ID:-9001}

   # `docker compose restart` / `docker restart` reuse the same container, so
   # the overlay fs keeps the user created on first boot. A second `useradd`
   # then fails with exit code 9, which would trip `set -e` and kill the
   # container on every restart. Create only if missing; align the UID
   # otherwise so LOCAL_USER_ID changes still apply.
   if ! id user > /dev/null 2>&1
   then
      useradd --shell /bin/bash -u ${USER_ID} -o -c "" -m user > /dev/null 2>&1 || true
   else
      usermod -u ${USER_ID} -o user > /dev/null 2>&1 || true
   fi
   usermod -a -G root user > /dev/null 2>&1 || true
   # Bind-mounted volumes may not support chown; a failure here must not kill
   # the container either.
   chown -R `id -u user`:`id -u user` /app > /dev/null 2>&1 || true

   export HOME=/home/user
   # setpriv (util-linux, present in bookworm-slim) replaces gosu: drop to the
   # target user and exec, keeping the process as PID 1.
   exec setpriv --reuid=`id -u user` --regid=`id -g user` --init-groups "$@"
fi

exec "$@"
