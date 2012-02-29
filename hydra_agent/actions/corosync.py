# ==============================
# Copyright 2011 Whamcloud, Inc.
# ==============================

"""
Corosync verification
"""

import socket
from hydra_agent.plugins import AgentPlugin
from hydra_agent import shell


def verify_corosync(args = None):
    """Verifies the state of corosync"""

    # Is corosync accessible?
    rc, stdout, stderr = shell.run("crm status", shell=True)
    if rc != 0:
        return {'accessible': False,
                'cluster_member': True,
                'targets_exist': False
               }

    # Is this node a member of a cluster?
    hostname = socket.gethostname()
    rc, stdout, stderr = shell.run("crm node show %s" % hostname,
                                       shell=True)
    if rc != 0 or not "%s: normal" % hostname in stdout:
        return {'accessible': True,
                'cluster_member': False,
                'targets_exist': False
               }

    # now make sure there are no resources using the Target ocf
    rc, stdout, stderr = shell.run("crm resource list", shell=True)
    for line in stdout.split('\n'):
        if "ocf::hydra:Target" in line:
            return {'accessible': True,
                    'cluster_member': True,
                    'targets_exist': True
                   }


class CorosyncPlugin(AgentPlugin):
    def register_commands(self, parser):
        p = parser.add_parser("corosync-sanity",
                              help="confirm corosync sanity")
        p.set_defaults(func=verify_corosync)
