# A script that handles translating our json input into the format
# accepted by our vm provisioner.
#
# python provisioner_json2test_json.py input_file.json output_file.json


import json
import sys

provisioner_json_file = open(sys.argv[1])
config = json.load(provisioner_json_file)
provisioner_json_file.close()

mcast_groups = {}


def mcast_port(vm_host_ip, cluster_number, bridge):
    if not bridge in mcast_groups:
        octets = [int(o) for o in vm_host_ip.split('.')]
        mcast_base = octets[0] + 6000 + octets[1] + octets[2] + octets[3] * cluster_number * 30
        group_port = len(mcast_groups) * 2
        mcast_groups[bridge] = str(mcast_base + group_port)
    return mcast_groups[bridge]


def setup_power_config():
    # NB: This will be wrong if there's ever more than one...
    vm_host_ipaddr = config['hosts'].values()[0]['ip_address']

    try:
        for pdu in config['power_distribution_units']:
            pdu['address'] = vm_host_ipaddr

        for server in config['lustre_servers']:
            fqdn = server['fqdn']
            identifier = fqdn[0:fqdn.find('.')] if fqdn.find('.') > 0 else fqdn
            config['pdu_outlets'].append({
                'host': fqdn,
                'identifier': identifier,
                'pdu': "%s:22" % vm_host_ipaddr
            })
    except KeyError:
        pass


def setup_corosync_config():
    # NB: This will be wrong if there's ever more than one...
    vm_host_ipaddr = config['hosts'].values()[0]['ip_address']
    try:
        cluster_num = config['hosts'].values()[0]['cluster_num'] + 1
    except KeyError:
        cluster_num = 1

    try:
        for server in config['lustre_servers']:
            # NB: I don't think we'll ever have > 1 bridge/server, but if we do,
            # this'll need to be updated.
            bridge = server['bridges'][0]
            server['corosync_config']['mcast_port'] = mcast_port(vm_host_ipaddr,
                                                                 cluster_num,
                                                                 bridge)
    except KeyError:
        pass

# We stopped using nodename and so it should not exist in the config, to ensure it doesn't creep backin.
# However for older branches the provisioner will have supplied it, so remove it here.
if config.get('lustre_servers'):
    for server in config['lustre_servers']:
        server.pop('nodename', None)

# until the provisioner is actually telling us which hosts have buggy reset
# implementations, assume if we're not told, it does
for host in config['hosts'].values():
    if not host.get('reset_is_buggy', False):
        host['reset_is_buggy'] = True

if config.get('chroma_managers') and config.get('simulator'):
    for manager in config['chroma_managers']:
        manager['server_http_url'] = "%s:8000/" % manager['server_http_url']

if config.get('lustre_servers'):
    for server in config['lustre_servers']:
        nodename = server['fqdn'].split('.')[0]
        start_command = server.get('start_command', None)
        if start_command and start_command.startswith("virsh start"):
            # until the provisioner is giving us an idempotent start
            # command adjust them so they are so
            server['start_command'] = '%s || [ "$(virsh domstate %s)" = "running" ]' % \
                (start_command, nodename)
            # and until the server is providing a reset command
            # create our own
            server['reset_command'] = \
                start_command.replace("start", "reset")
        # until the provisioner is giving us an idempotent destroy
        # command adjust them so they are so
        destroy_command = server.get('destroy_command', None)
        if destroy_command and destroy_command.startswith("virsh destroy"):
            server['destroy_command'] = '[ "$(virsh domstate %s)" = "shut off" ] || %s' % \
                (nodename, destroy_command)


if not config.get('simulator', False):
    setup_power_config()
    setup_corosync_config()

if config.get('filesystem') and config.get('lustre_servers'):
    for name, target in config['filesystem']['targets'].iteritems():
        if target['kind'] in ['MGT', 'MDT']:
            target['primary_server'] = config['lustre_servers'][0]['fqdn']
        if target['kind'] in ['OST', 'OSTorMDT']:
            # Split up the osts between the lustre servers
            target['primary_server'] = config['lustre_servers'][(int(name[-1]) % 2) + 1]['fqdn']

test_json_file = open(sys.argv[2], 'w')
json.dump(config, test_json_file, sort_keys=True, indent=4)
test_json_file.close()
