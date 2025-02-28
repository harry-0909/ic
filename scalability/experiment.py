import argparse
import json
import os
import re
import subprocess
import sys
import time
import traceback
from typing import List

import flamegraphs
import generate_report
import gflags
import machine_failure
import prometheus
import ssh

NNS_SUBNET_INDEX = 0  # Subnet index of the NNS subnetwork

FLAGS = gflags.FLAGS
gflags.DEFINE_string("testnet", None, 'Testnet to use. Use "mercury" to run against mainnet.')
gflags.MarkFlagAsRequired("testnet")
gflags.DEFINE_boolean("skip_generate_report", False, "Skip generating report after experiment is finished")
gflags.DEFINE_boolean("should_deploy_ic", False, "Should the IC be deployed on testnet before the experiment.")
gflags.DEFINE_string("git_revision", "", "Git revision to be deployed to the testnet")
gflags.DEFINE_string("canister_id", "", "Use given canister ID instead of installing a new canister")
gflags.DEFINE_string("artifacts_path", "../artifacts/release", "Path to the artifacts directory")
gflags.DEFINE_boolean("no_instrument", False, "Do not instrument target machine")
gflags.DEFINE_string("top_level_out_dir", "", "Set the top-level output directory. Default is the git commit id.")
gflags.DEFINE_string(
    "second_level_out_dir",
    "",
    "Set the second-level output directory. Default is the UNIX timestamp at benchmark start.",
)

gflags.DEFINE_boolean("simulate_machine_failures", False, "Simulate machine failures while testing.")
gflags.DEFINE_string("nns_url", "", "Use the following NNS URL instead of getting it from the testnet configuration")


class Color:
    """Colors for the shell commands."""

    BLUE = "\033[94m"
    RED = "\033[91m"
    BOLD = "\033[1m"
    END = "\033[0m"
    GREEN = "\033[32m"


def try_deploy_ic(testnet: str, revision: str, out_dir: str) -> None:
    """
    Try to deploy IC on the desired testnet.

    Args:
    ----
        testnet (str): name of the testnet, e.g. large01.
        revision (str): git revision hash to be used to deploy.
        out_dir (str): directory for storing stdout and stderr into files.

    """
    # TODO: command paths should be managed better.
    # Get the newest hash (containing disk image) from master.
    result_stdout = f"{out_dir}/stdout_log.txt"
    result_stderr = f"{out_dir}/stderr_log.txt"

    if revision == "":
        with open(result_stderr, "w") as errfile:
            try:
                result_newest_revision = subprocess.run(
                    ["../gitlab-ci/src/artifacts/newest_sha_with_disk_image.sh", "origin/master"],
                    stdout=subprocess.PIPE,
                    stderr=errfile,
                )
            except Exception as e:
                print(f"Getting newest revision failed. See {result_stderr} file for details.")
                errfile.write(str(e))
                errfile.write(traceback.format_exc())
                sys.exit(1)
            if result_newest_revision.returncode != 0:
                print(f"Getting newest revision failed. See {result_stderr} file for details.")
                sys.exit(1)
        revision = result_newest_revision.stdout.decode("utf-8").strip()

    # Start the IC deployment.
    print(
        f"{Color.RED}Deploying IC revision {revision} on testnet={testnet}. See the intermediate output in {result_stdout}. This can take some minutes ...{Color.END}"
    )

    with open(result_stdout, "w") as outfile, open(result_stderr, "w") as errfile:
        try:
            result_deploy_ic = subprocess.run(
                ["../testnet/tools/icos_deploy.sh", "--git-revision", f"{revision}", f"{testnet}"],
                stdout=outfile,
                stderr=errfile,
            )
        except Exception as e:
            print(f"Deployment of the IC failed: See {result_stderr} file for details.")
            errfile.write(str(e))
            errfile.write(traceback.format_exc())
            sys.exit(1)
    if result_deploy_ic.returncode != 0:
        print(f"Deployment of the IC failed. See {result_stderr} file for details.")
        sys.exit(1)
    print(f"{Color.BOLD}{Color.GREEN}Deployment of the IC to testnet={testnet} finished successfully.{Color.END}")


def parse_command_line_args():
    # Start: Provide command line args support #
    # Get a dictionary of gflags from all imported files.
    flags = gflags.FLAGS.__dict__["__flags"]

    parser = argparse.ArgumentParser(description=f"{Color.BOLD}{Color.BLUE}Experiment parameters.{Color.END}")
    # Create a set of command line options, based on the imported gflags.
    for key, value in flags.items():
        if key == "help":
            continue
        # gflags with default=None are required arguments. (SK: that's not true, optional flags with None as default values are not required)
        if value.default is None:
            parser.add_argument(f"--{key}", required=True, help=f"{Color.RED} Required field. {Color.END} {value.help}")
        else:
            parser.add_argument(
                f"--{key}", required=False, default=value.default, help=f"{value.help}; default={value.default}"
            )
    # Now useful help message can be queried via: `python script_name.py -h`
    parser.parse_args()
    # Initialize gflags from the command line args.
    FLAGS(sys.argv)
    # Print all gflags for the experiment.
    print(f"{Color.BOLD}{Color.RED}The following values will " f"be used in the experiment.{Color.END}")
    for key, value in flags.items():
        print(f"Parameter {Color.BLUE} {key} = {value.value} {Color.END}")

    if FLAGS.testnet == "mercury" and FLAGS.target_subnet_id is None:
        raise Exception("--target_subnet_id has to be set when running against mainnet")

    # End: Provide command line args support #


class Experiment:
    """Wrapper class around experiments."""

    def __init__(self, request_type="query"):
        """Init."""
        sys.path.insert(1, "../ic-os/guestos/tests")  # for ictools
        import ictools

        self.load_artifacts()

        self.testnet = FLAGS.testnet
        self.canister_ids = []
        self.canister = None
        self.metrics = []

        self.t_experiment_start = None
        self.iteration = 0

        self.request_type = request_type
        self.git_hash = (
            FLAGS.git_revision
            if FLAGS.git_revision != ""
            else ictools.get_ic_version("http://[{}]:8080/api/v2/status".format(self.get_machine_to_instrument()))
        )

    def init(self):
        """Initialize experiment."""
        print(f"Running against an IC with git hash: {self.git_hash}")

        self.out_dir_timestamp = int(time.time())
        self.out_dir = "{}/{}/".format(
            self.git_hash if len(FLAGS.top_level_out_dir) < 1 else FLAGS.top_level_out_dir,
            self.out_dir_timestamp if len(FLAGS.second_level_out_dir) < 1 else FLAGS.second_level_out_dir,
        )
        os.makedirs(self.out_dir, 0o755)
        print(f"📂 Storing output in {self.out_dir}")

        if FLAGS.should_deploy_ic:
            try_deploy_ic(testnet=self.testnet, revision=FLAGS.git_revision, out_dir=self.out_dir)

        self.store_ic_info()
        self.store_hardware_info()

    def load_artifacts(self):
        """
        Load artifacts.

        If previously downloaded, reuse, otherwise download.
        When downloading, store the GIT commit hash that has been used in a text file.
        """
        self.artifacts_path = FLAGS.artifacts_path
        f_artifacts_hash = os.path.join(self.artifacts_path, "githash")
        if subprocess.run(["stat", f_artifacts_hash]).returncode != 0:
            print("Could not find artifacts, downloading .. ")
            # Delete old artifacts directory, if it exists
            subprocess.run(["rm", "-rf", self.artifacts_path], check=True)
            # Download new artifacts.
            artifacts_env = os.environ.copy()
            artifacts_env["GIT"] = subprocess.check_output(["git", "rev-parse", "origin/master"], encoding="utf-8")
            artifacts_env["GET_GUEST_OS"] = "0"
            output = subprocess.check_output(
                ["../ic-os/guestos/scripts/get-artifacts.sh"], encoding="utf-8", env=artifacts_env
            )
            match = re.findall(r"Downloading artifacts for revision ([a-f0-9]*)", output)[0]
            f = open(f_artifacts_hash, "wt", encoding="utf-8")
            f.write(match)
        else:
            print(
                (
                    "⚠️  Re-using artifacts. While this is faster, there is a risk of inconsistencies."
                    f'Call "rm -rf {self.artifacts_path}" in case something doesn\'t work.'
                )
            )
        self.artifacts_hash = open(f_artifacts_hash, "r").read()

        print(f"Artifacts hash is {self.artifacts_hash}")
        print(f"Found artifacts at {self.artifacts_path}")
        self.workload_generator_path = os.path.join(self.artifacts_path, "ic-workload-generator")

    def get_machine_to_instrument(self) -> str:
        """Return the machine to instrument."""
        res = subprocess.check_output(
            [self.get_ic_admin_path(), "--nns-url", self.get_nns_url(), "get-topology"], encoding="utf-8"
        )
        for subnet, info in json.loads(res)["topology"]["subnets"].items():
            subnet_type = info["records"][0]["value"]["subnet_type"]
            members = info["records"][0]["value"]["membership"]
            if subnet_type == "application":
                return self.get_node_ip_address(members[0])

    def get_subnet_to_instrument(self) -> str:
        """Return the subnet to instrument."""
        res = subprocess.check_output(
            [self.get_ic_admin_path(), "--nns-url", self.get_nns_url(), "get-topology"], encoding="utf-8"
        )
        for subnet, info in json.loads(res)["topology"]["subnets"].items():
            subnet_type = info["records"][0]["value"]["subnet_type"]
            if subnet_type == "application":
                return subnet

    def run_experiment(self, config):
        """Run a single iteration of the experiment."""
        self.start_iteration()
        result = self.run_experiment_internal(config)
        self.end_iteration(config)
        return result

    def run_experiment_internal(self, config):
        """Run a single iteration of the experiment."""
        raise Exception("Needs to be implemented by each experiment")

    def init_metrics(self):
        """Initialize metrics to collect for experiment."""
        self.metrics = [
            flamegraphs.Flamegraph("flamegraph", self.get_machine_to_instrument(), not FLAGS.no_instrument),
            prometheus.Prometheus("prometheus", self.get_machine_to_instrument(), not FLAGS.no_instrument),
        ]
        for m in self.metrics:
            m.init()

    def init_experiment(self):
        """Initialize what's necessary to run experiments."""
        self.init_metrics()

    def start_iteration(self):
        """Start a new iteration of the experiment."""
        self.iteration += 1
        self.t_iter_start = int(time.time())

        # Create output directory
        self.iter_outdir = "{}/{}".format(self.out_dir, self.iteration)
        os.makedirs(self.iter_outdir, 0o755)

        if FLAGS.simulate_machine_failures:
            machine_failure.MachineFailure(self).start()

        # Start metrics for this iteration
        for m in self.metrics:
            m.start_iteration(self.iter_outdir)

    def end_iteration(self, configuration={}):
        """End a new iteration of the experiment."""
        self.t_iter_end = int(time.time())

        # Get logs from targets
        since_time = self.t_iter_end - self.t_iter_start
        self.get_iter_logs_from_targets(self.target_nodes, f"-{since_time}", self.iter_outdir)

        for m in self.metrics:
            m.end_iteration(self)

        # Dump experiment info
        with open(os.path.join(self.iter_outdir, "iteration.json"), "w") as iter_file:
            iter_file.write(
                json.dumps(
                    {
                        "t_start": self.t_iter_start,
                        "t_end": self.t_iter_end,
                        "configuration": configuration,
                    }
                )
            )

    def start_experiment(self):
        """Start the experiment."""
        self.t_experiment_start = int(time.time())

    def end_experiment(self):
        """End the experiment."""
        print(
            "Experiment finished. Generating report like: python3 generate_report.py {} {}".format(
                self.git_hash, self.out_dir_timestamp
            )
        )
        if not FLAGS.skip_generate_report:
            generate_report.generate_report(self.git_hash, self.out_dir_timestamp)

    def get_ic_admin_path(self):
        """Return path to ic-admin."""
        return os.path.join(self.artifacts_path, "ic-admin")

    def get_topology(self):
        """Get the current topology from the registry."""
        res = subprocess.check_output(
            [self.get_ic_admin_path(), "--nns-url", self.get_nns_url(), "get-topology"], encoding="utf-8"
        )
        return json.loads(res)

    def get_node_info(self, nodeid):
        """Get info for the given node from the registry."""
        return subprocess.check_output(
            [self.get_ic_admin_path(), "--nns-url", self.get_nns_url(), "get-node", nodeid], encoding="utf-8"
        )

    def get_subnet_info(self, subnet_idx):
        """Get info for the given subnet from the registry."""
        return subprocess.check_output(
            [self.get_ic_admin_path(), "--nns-url", self.get_nns_url(), "get-subnet", str(subnet_idx)], encoding="utf-8"
        )

    def store_ic_info(self):
        """Store subnet info for the subnet that we are targeting in the experiment output directory."""
        jsondata = self.get_subnet_info(self.get_subnet_to_instrument())
        with open(os.path.join(self.out_dir, "subnet_info.json"), "w") as subnet_file:
            subnet_file.write(jsondata)

        jsondata = self.get_topology()
        with open(os.path.join(self.out_dir, "topology.json"), "w") as subnet_file:
            subnet_file.write(json.dumps(jsondata, indent=2))

    def store_hardware_info(self):
        """Store info for the target machine in the experiment output directory."""
        if FLAGS.no_instrument:
            return
        p = ssh.run_ssh(
            self.get_machine_to_instrument(),
            "lscpu",
            f_stdout=os.path.join(self.out_dir, "lscpu.stdout.txt"),
            f_stderr=os.path.join(self.out_dir, "lscpu.stderr.txt"),
        )
        p.wait()

        p = ssh.run_ssh(
            self.get_machine_to_instrument(),
            "free -h",
            f_stdout=os.path.join(self.out_dir, "free.stdout.txt"),
            f_stderr=os.path.join(self.out_dir, "free.stderr.txt"),
        )
        p.wait()

    def get_node_ip_address(self, nodeid):
        """Get HTTP endpoint for the given node."""
        nodeinfo = self.get_node_info(nodeid)
        ip = re.findall(r'ip_addr: "([a-f0-9:A-F]+)"', nodeinfo)
        return ip[0]

    def get_unassigned_nodes(self):
        """Return a list of unassigned node IDs in the given subnetwork."""
        topo = self.get_topology()
        return [j["node_id"] for j in topo["topology"]["unassigned_nodes"]]

    def get_subnets(self):
        """Get the currently running subnetworks."""
        topo = self.get_topology()
        return [k for (k, _) in topo["topology"]["subnets"].items()]

    def get_subnet_members(self, subnet_index):
        """Get members of subnet with the given subnet index (not subnet ID)."""
        topo = self.get_topology()
        subnet_info = [info for (_, info) in topo["topology"]["subnets"].items()]
        return subnet_info[subnet_index]["records"][0]["value"]["membership"]

    def get_nns_url(self):
        """Get the testnets NNS url."""
        if len(FLAGS.nns_url) > 0:
            return FLAGS.nns_url
        ip = (
            "2001:920:401a:1708:5000:4fff:fe92:48f1"
            if FLAGS.testnet == "mercury"
            else self.get_hostnames(FLAGS.testnet, NNS_SUBNET_INDEX)[0]
        )
        return f"http://[{ip}]:8080"

    def add_node_to_subnet(self, subnet_index, node_ids):
        """Add nodes given in node_ids to the given subnetwork."""
        assert isinstance(node_ids, list)
        processes = []
        for node_id in node_ids:
            cmd = [
                self.get_ic_admin_path(),
                "--nns-url",
                self.get_nns_url(),
                "propose-to-add-nodes-to-subnet",
                "--test-neuron-proposer",
                "--subnet-id",
                str(subnet_index),
                node_id,
            ]
            print(f"Executing {cmd}")
            p = subprocess.Popen(cmd)
            processes.append(p)

        for p in processes:
            p.wait()

        num_tries = 0
        node_added = False
        while node_added:

            print(f"Testing if node {node_id} is a member of subnet {subnet_index}")
            num_tries += 1
            assert num_tries < 10  # Otherwise timeout

            node_added = True

            for node_id in node_ids:
                node_added &= node_id in self.get_subnet_members(subnet_index)

    def turn_off_replica(self, machines):
        """Turn of replicas on the given machines."""
        for m in machines:
            print(f"💣 Stopping machine {m}")
        return ssh.run_ssh_in_parallel(machines, "sudo systemctl stop ic-replica")

    def install_canister_nonblocking(self, target, canister=None):
        """
        Install the canister on the given machine.

        Note that canisters are currently always installed as best effort.
        """
        print("Installing canister .. ")
        cmd = [self.workload_generator_path, "http://[{}]:8080".format(target), "-n", "1", "-r", "1"]
        if canister is not None:
            cmd += ["--canister", canister]
        return subprocess.Popen(cmd, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)

    def install_canister(self, target: str, canister=None, check=True):
        """
        Install the canister on the given machine.

        Note that canisters are currently always installed as best effort.
        """
        if FLAGS.canister_id is not None and len(FLAGS.canister_id) > 0:
            print(f"⚠️  Not installing canister, using {FLAGS.canister_id} ")
            self.canister = f"pre-installed canister {FLAGS.canister_id}"
            self.canister_ids.extend(FLAGS.canister_id.split(","))
            return

        print("Installing canister .. ")
        self.canister = canister if canister is not None else "counter"
        cmd = [self.workload_generator_path, f"http://[{target}]:8080", "-n", "1", "-r", "1"]
        if canister is not None:
            cmd += ["--canister", canister]
        try:
            p = subprocess.run(
                cmd,
                check=check,
                capture_output=True,
            )
            wg_output = p.stdout.decode("utf-8").strip()
            for line in wg_output.split("\n"):
                print("Output: ", line)
                canister_id = re.findall(r"Successfully created canister at URL [^ ]*. ID: [^ ]*", line)
                if len(canister_id):
                    cid = canister_id[0].split()[7]
                    self.canister_ids.append(cid)
                    print("Found canister ID: ", cid)
            wg_err_output = p.stderr.decode("utf-8").strip()
            for line in wg_err_output.split("\n"):
                print("Output (stderr):", line)
        except Exception as e:
            print(f"Failed to install canister, return code: {e.returncode}")
            print(f"Command was: {cmd}")
            print(e.output.decode("utf-8"))
            print(e.stderr.decode("utf-8"))
            exit(5)

    def get_machines(self, testnet, subnet=0):
        """Get a list of machines for the given subnetwork."""
        p = subprocess.run(
            ["ansible-inventory", "-i", "env/{}/hosts".format(testnet), "--list"],
            check=True,
            cwd="../testnet",
            capture_output=True,
        )
        j = json.loads(p.stdout.decode("utf-8"))

        hosts = [
            info
            for (_, info) in j["_meta"]["hostvars"].items()
            if "subnet_index" in info and info["subnet_index"] == subnet
        ]

        return hosts

    def get_hostnames(self, testnet, subnet=0):
        """Return hostnames of all machines in the given testnet and subnet."""
        return sorted([h["ansible_host"] for h in self.get_machines(testnet, subnet)])

    def build_summary_file(self):
        """Build dictionary to be used to build the summary file."""
        return {}

    def write_summary_file(
        self, experiment_name, experiment_details, xlabels, xtitle="n.a.", rtype="query", state="running"
    ):
        """
        Write the current summary file.

        The idea is that we write one after each iteration, so that we can
        generate reports from intermediate versions.
        """
        d = self.build_summary_file()
        d.update(
            {
                "xlabels": xlabels,
                "xtitle": xtitle,
                "command_line": sys.argv,
                "experiment_name": experiment_name,
                "experiment_details": experiment_details,
                "type": rtype,
                "workload": self.canister,
                "testnet": self.testnet,
                "user": subprocess.check_output(["whoami"], encoding="utf-8"),
                "canister_id": self.canister_ids,
                "artifacts_githash": self.artifacts_hash,
                "t_experiment_start": self.t_experiment_start,
                "t_experiment_end": int(time.time()),
                "state": state,
            }
        )
        with open(os.path.join(self.out_dir, "experiment.json"), "w") as iter_file:
            iter_file.write(json.dumps(d))

    def get_iter_logs_from_targets(self, machines: List[str], since_time: str, outdir: str):
        """Fetch logs from target machines since the given time."""
        ssh.run_all_ssh_in_parallel(
            machines,
            [f"journalctl -u ic-replica --since={since_time}" for m in machines],
            outdir + "/replica-log-{}-stdout.txt",
            outdir + "/replica-log-{}-stderr.txt",
        )
